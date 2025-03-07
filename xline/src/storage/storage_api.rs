use curp::cmd::ProposeId;

use super::{db::WriteOp, ExecuteError};

/// The Stable Storage Api
pub trait StorageApi: Send + Sync + 'static + std::fmt::Debug {
    /// Get values by keys from storage
    ///
    /// # Errors
    ///
    /// if error occurs in storage, return `Err(error)`
    fn get_values<K>(
        &self,
        table: &'static str,
        keys: &[K],
    ) -> Result<Vec<Option<Vec<u8>>>, ExecuteError>
    where
        K: AsRef<[u8]> + std::fmt::Debug;

    /// Get values by keys from storage
    ///
    /// # Errors
    ///
    /// if error occurs in storage, return `Err(error)`
    fn get_value<K>(&self, table: &'static str, key: K) -> Result<Option<Vec<u8>>, ExecuteError>
    where
        K: AsRef<[u8]> + std::fmt::Debug;

    /// Get all values of the given table from storage
    ///
    /// # Errors
    ///
    /// if error occurs in storage, return `Err(error)`
    #[allow(clippy::type_complexity)] // it's clear that (Vec<u8>, Vec<u8>) is a key-value pair
    fn get_all(&self, table: &'static str) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ExecuteError>;

    /// Reset the storage
    ///
    /// # Errors
    ///
    /// if error occurs in storage, return `Err(error)`
    fn reset(&self) -> Result<(), ExecuteError>;

    /// Put a write operation to the buffer
    fn buffer_op(&self, id: &ProposeId, op: WriteOp);

    /// Flush the buffer to storage
    fn flush(&self, id: &ProposeId) -> Result<(), ExecuteError>;
}

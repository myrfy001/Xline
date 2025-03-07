/// Lease
mod lease;
/// Lease heap
mod lease_queue;
/// Lease cmd, used by other storages
mod message;

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use clippy_utilities::Cast;
use curp::cmd::ProposeId;
use log::debug;
use parking_lot::RwLock;
use prost::Message;
use tokio::sync::mpsc;

use self::lease_queue::LeaseQueue;
pub(crate) use self::{lease::Lease, message::LeaseMessage};
use super::{
    db::WriteOp,
    index::{Index, IndexOperate},
    kv_store::KV_TABLE,
    storage_api::StorageApi,
    ExecuteError,
};
use crate::{
    header_gen::HeaderGenerator,
    revision_number::RevisionNumber,
    rpc::{
        Event, EventType, KeyValue, LeaseGrantRequest, LeaseGrantResponse, LeaseRevokeRequest,
        LeaseRevokeResponse, PbLease, RequestWithToken, RequestWrapper, ResponseHeader,
        ResponseWrapper,
    },
    server::command::{CommandResponse, SyncResponse},
    state::State,
    storage::Revision,
};

/// Lease table name
pub(crate) const LEASE_TABLE: &str = "lease";
/// Max lease ttl
const MAX_LEASE_TTL: i64 = 9_000_000_000;
/// Min lease ttl
const MIN_LEASE_TTL: i64 = 1; // TODO: this num should calculated by election ticks and heartbeat

/// Lease store
#[derive(Debug)]
pub(crate) struct LeaseStore<DB>
where
    DB: StorageApi,
{
    /// Lease store Backend
    inner: Arc<LeaseStoreBackend<DB>>,
}

/// Collection of lease related data
#[derive(Debug)]
struct LeaseCollection {
    /// lease id to lease
    lease_map: HashMap<i64, Lease>,
    /// key to lease id
    item_map: HashMap<Vec<u8>, i64>,
    /// lease queue
    expired_queue: LeaseQueue,
}

impl LeaseCollection {
    /// New `LeaseCollection`
    fn new() -> Self {
        Self {
            lease_map: HashMap::new(),
            item_map: HashMap::new(),
            expired_queue: LeaseQueue::new(),
        }
    }

    /// Find expired leases
    fn find_expired_leases(&mut self) -> Vec<i64> {
        let mut expired_leases = vec![];
        while let Some(expiry) = self.expired_queue.peek() {
            if *expiry <= Instant::now() {
                #[allow(clippy::unwrap_used)] // queue.peek() returns Some
                let id = self.expired_queue.pop().unwrap();
                if self.lease_map.contains_key(&id) {
                    expired_leases.push(id);
                }
            } else {
                break;
            }
        }
        expired_leases
    }

    /// Renew lease
    fn renew(&mut self, lease_id: i64) -> Result<i64, ExecuteError> {
        self.lease_map.get_mut(&lease_id).map_or_else(
            || Err(ExecuteError::lease_not_found(lease_id)),
            |lease| {
                if lease.expired() {
                    return Err(ExecuteError::lease_expired(lease_id));
                }
                let expiry = lease.refresh(Duration::default());
                let _ignore = self.expired_queue.update(lease_id, expiry);
                Ok(lease.ttl().as_secs().cast())
            },
        )
    }

    /// Attach key to lease
    fn attach(&mut self, lease_id: i64, key: Vec<u8>) -> Result<(), ExecuteError> {
        self.lease_map.get_mut(&lease_id).map_or_else(
            || Err(ExecuteError::lease_not_found(lease_id)),
            |lease| {
                lease.insert_key(key.clone());
                let _ignore = self.item_map.insert(key, lease_id);
                Ok(())
            },
        )
    }

    /// Detach key from lease
    fn detach(&mut self, lease_id: i64, key: &[u8]) -> Result<(), ExecuteError> {
        self.lease_map.get_mut(&lease_id).map_or_else(
            || Err(ExecuteError::lease_not_found(lease_id)),
            |lease| {
                lease.remove_key(key);
                let _ignore = self.item_map.remove(key);
                Ok(())
            },
        )
    }

    /// Check if a lease exists
    fn contains_lease(&self, lease_id: i64) -> bool {
        self.lease_map.contains_key(&lease_id)
    }

    /// Grant a lease
    fn grant(&mut self, lease_id: i64, ttl: i64, is_leader: bool) -> PbLease {
        let mut lease = Lease::new(lease_id, ttl.max(MIN_LEASE_TTL).cast());
        if is_leader {
            let expiry = lease.refresh(Duration::ZERO);
            let _ignore = self.expired_queue.insert(lease_id, expiry);
        } else {
            lease.forever();
        }
        let _ignore = self.lease_map.insert(lease_id, lease.clone());
        PbLease {
            id: lease.id(),
            ttl: lease.ttl().as_secs().cast(),
            remaining_ttl: lease.remaining_ttl().as_secs().cast(),
        }
    }

    /// Revokes a lease
    fn revoke(&mut self, lease_id: i64) -> Option<Lease> {
        self.lease_map.remove(&lease_id)
    }

    /// Demote current node
    fn demote(&mut self) {
        self.lease_map.values_mut().for_each(Lease::forever);
        self.expired_queue.clear();
    }

    /// Promote current node
    fn promote(&mut self, extend: Duration) {
        for lease in self.lease_map.values_mut() {
            let expiry = lease.refresh(extend);
            let _ignore = self.expired_queue.insert(lease.id(), expiry);
        }
    }
}

/// Lease store inner
#[derive(Debug)]
pub(crate) struct LeaseStoreBackend<DB>
where
    DB: StorageApi,
{
    /// lease collection
    lease_collection: RwLock<LeaseCollection>,
    /// Db to store lease
    db: Arc<DB>,
    /// Key to revision index
    index: Arc<Index>,
    /// Current node is leader or not
    state: Arc<State>,
    /// Revision
    revision: Arc<RevisionNumber>,
    /// Header generator
    header_gen: Arc<HeaderGenerator>,
    /// KV update sender
    kv_update_tx: mpsc::Sender<(i64, Vec<Event>)>,
}

impl<DB> LeaseStore<DB>
where
    DB: StorageApi,
{
    /// New `LeaseStore`
    #[allow(clippy::integer_arithmetic)] // Introduced by tokio::select!
    pub(crate) fn new(
        mut lease_cmd_rx: mpsc::Receiver<LeaseMessage>,
        state: Arc<State>,
        header_gen: Arc<HeaderGenerator>,
        db: Arc<DB>,
        index: Arc<Index>,
        kv_update_tx: mpsc::Sender<(i64, Vec<Event>)>,
    ) -> Self {
        let inner = Arc::new(LeaseStoreBackend::new(
            state,
            header_gen,
            db,
            index,
            kv_update_tx,
        ));
        let _handle = tokio::spawn({
            let inner = Arc::clone(&inner);
            async move {
                while let Some(lease_msg) = lease_cmd_rx.recv().await {
                    match lease_msg {
                        LeaseMessage::Attach(tx, lease_id, key) => {
                            assert!(
                                tx.send(inner.attach(lease_id, key)).is_ok(),
                                "receiver is closed"
                            );
                        }
                        LeaseMessage::Detach(tx, lease_id, key) => {
                            assert!(
                                tx.send(inner.detach(lease_id, &key)).is_ok(),
                                "receiver is closed"
                            );
                        }
                        LeaseMessage::GetLease(tx, key) => {
                            assert!(tx.send(inner.get_lease(&key)).is_ok(), "receiver is closed");
                        }
                        LeaseMessage::LookUp(tx, lease_id) => {
                            assert!(
                                tx.send(inner.look_up(lease_id)).is_ok(),
                                "receiver is closed"
                            );
                        }
                    }
                }
            }
        });
        Self { inner }
    }

    /// execute a lease request
    pub(crate) fn execute(
        &self,
        request: &RequestWithToken,
    ) -> Result<CommandResponse, ExecuteError> {
        self.inner
            .handle_lease_requests(&request.request)
            .map(CommandResponse::new)
    }

    /// sync a lease request
    pub(crate) async fn after_sync(
        &self,
        id: &ProposeId,
        request: &RequestWithToken,
    ) -> Result<SyncResponse, ExecuteError> {
        self.inner
            .sync_request(id, &request.request)
            .await
            .map(SyncResponse::new)
    }

    /// Check if the node is leader
    fn is_leader(&self) -> bool {
        self.inner.is_leader()
    }

    /// Get lease by id
    pub(crate) fn look_up(&self, lease_id: i64) -> Option<Lease> {
        self.inner.look_up(lease_id)
    }

    /// Get all leases
    pub(crate) fn leases(&self) -> Vec<Lease> {
        let mut leases = self
            .inner
            .lease_collection
            .read()
            .lease_map
            .values()
            .cloned()
            .collect::<Vec<_>>();
        leases.sort_by_key(Lease::remaining);
        leases
    }

    /// Find expired leases
    pub(crate) fn find_expired_leases(&self) -> Vec<i64> {
        self.inner.lease_collection.write().find_expired_leases()
    }

    /// Get keys attached to a lease
    pub(crate) fn get_keys(&self, lease_id: i64) -> Vec<Vec<u8>> {
        self.inner
            .lease_collection
            .read()
            .lease_map
            .get(&lease_id)
            .map(Lease::keys)
            .unwrap_or_default()
    }

    /// Keep alive a lease
    pub(crate) fn keep_alive(&self, lease_id: i64) -> Result<i64, ExecuteError> {
        if !self.is_leader() {
            return Err(ExecuteError::lease_not_leader());
        }
        self.inner.lease_collection.write().renew(lease_id)
    }

    /// Generate `ResponseHeader`
    pub(crate) fn gen_header(&self) -> ResponseHeader {
        self.inner.header_gen.gen_header()
    }

    /// Demote current node
    pub(crate) fn demote(&self) {
        self.inner.lease_collection.write().demote();
    }

    /// Promote current node
    pub(crate) fn promote(&self, extend: Duration) {
        self.inner.lease_collection.write().promote(extend);
    }

    /// Recover data form persistent storage
    pub(crate) fn recover(&self) -> Result<(), ExecuteError> {
        self.inner.recover_from_current_db()
    }
}

impl<DB> LeaseStoreBackend<DB>
where
    DB: StorageApi,
{
    /// New `LeaseStoreBackend`
    pub(crate) fn new(
        state: Arc<State>,
        header_gen: Arc<HeaderGenerator>,
        db: Arc<DB>,
        index: Arc<Index>,
        kv_update_tx: mpsc::Sender<(i64, Vec<Event>)>,
    ) -> Self {
        Self {
            lease_collection: RwLock::new(LeaseCollection::new()),
            db,
            state,
            revision: header_gen.revision_arc(),
            header_gen,
            index,
            kv_update_tx,
        }
    }

    /// Check if the node is leader
    fn is_leader(&self) -> bool {
        self.state.is_leader()
    }

    /// Attach key to lease
    pub(crate) fn attach(&self, lease_id: i64, key: Vec<u8>) -> Result<(), ExecuteError> {
        self.lease_collection.write().attach(lease_id, key)
    }

    /// Detach key from lease
    pub(crate) fn detach(&self, lease_id: i64, key: &[u8]) -> Result<(), ExecuteError> {
        self.lease_collection.write().detach(lease_id, key)
    }

    /// Get lease id by given key
    pub(crate) fn get_lease(&self, key: &[u8]) -> i64 {
        self.lease_collection
            .read()
            .item_map
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    /// Get lease by id
    pub(crate) fn look_up(&self, lease_id: i64) -> Option<Lease> {
        self.lease_collection
            .read()
            .lease_map
            .get(&lease_id)
            .cloned()
    }

    /// Recover data form persistent storage
    fn recover_from_current_db(&self) -> Result<(), ExecuteError> {
        let leases = self.get_all()?;
        for lease in leases {
            let _ignore = self
                .lease_collection
                .write()
                .grant(lease.id, lease.ttl, false);
        }
        Ok(())
    }

    /// Handle lease requests
    fn handle_lease_requests(
        &self,
        wrapper: &RequestWrapper,
    ) -> Result<ResponseWrapper, ExecuteError> {
        debug!("Receive request {:?}", wrapper);
        #[allow(clippy::wildcard_enum_match_arm)]
        let res = match *wrapper {
            RequestWrapper::LeaseGrantRequest(ref req) => {
                debug!("Receive LeaseGrantRequest {:?}", req);
                self.handle_lease_grant_request(req).map(Into::into)
            }
            RequestWrapper::LeaseRevokeRequest(ref req) => {
                debug!("Receive LeaseRevokeRequest {:?}", req);
                self.handle_lease_revoke_request(req).map(Into::into)
            }
            _ => unreachable!("Other request should not be sent to this store"),
        };
        res
    }

    /// Handle `LeaseGrantRequest`
    fn handle_lease_grant_request(
        &self,
        req: &LeaseGrantRequest,
    ) -> Result<LeaseGrantResponse, ExecuteError> {
        if req.id == 0 {
            return Err(ExecuteError::lease_not_found(0));
        }
        if req.ttl > MAX_LEASE_TTL {
            return Err(ExecuteError::lease_ttl_too_large(req.ttl));
        }
        if self.lease_collection.read().contains_lease(req.id) {
            return Err(ExecuteError::lease_already_exists(req.id));
        }

        Ok(LeaseGrantResponse {
            header: Some(self.header_gen.gen_header_without_revision()),
            id: req.id,
            ttl: req.ttl,
            error: String::new(),
        })
    }

    /// Handle `LeaseRevokeRequest`
    fn handle_lease_revoke_request(
        &self,
        req: &LeaseRevokeRequest,
    ) -> Result<LeaseRevokeResponse, ExecuteError> {
        if self.lease_collection.read().contains_lease(req.id) {
            Ok(LeaseRevokeResponse {
                header: Some(self.header_gen.gen_header_without_revision()),
            })
        } else {
            Err(ExecuteError::lease_not_found(req.id))
        }
    }

    /// Sync `RequestWithToken`
    async fn sync_request(
        &self,
        id: &ProposeId,
        wrapper: &RequestWrapper,
    ) -> Result<i64, ExecuteError> {
        #[allow(clippy::wildcard_enum_match_arm)]
        match *wrapper {
            RequestWrapper::LeaseGrantRequest(ref req) => {
                debug!("Sync LeaseGrantRequest {:?}", req);
                self.sync_lease_grant_request(id, req);
            }
            RequestWrapper::LeaseRevokeRequest(ref req) => {
                debug!("Sync LeaseRevokeRequest {:?}", req);
                self.sync_lease_revoke_request(id, req).await?;
            }
            _ => unreachable!("Other request should not be sent to this store"),
        };
        Ok(self.header_gen.revision())
    }

    /// Sync `LeaseGrantRequest`
    fn sync_lease_grant_request(&self, id: &ProposeId, req: &LeaseGrantRequest) {
        let lease = self
            .lease_collection
            .write()
            .grant(req.id, req.ttl, self.is_leader());
        self.db.buffer_op(id, WriteOp::PutLease(lease));
    }

    /// Get all `PbLease`
    fn get_all(&self) -> Result<Vec<PbLease>, ExecuteError> {
        self.db
            .get_all(LEASE_TABLE)
            .map_err(|e| ExecuteError::DbError(format!("Failed to get all leases, error: {e}")))?
            .into_iter()
            .map(|(_, v)| {
                PbLease::decode(&mut v.as_slice()).map_err(|e| {
                    ExecuteError::DbError(format!("Failed to decode lease, error: {e}"))
                })
            })
            .collect()
    }

    /// Sync `LeaseRevokeRequest`
    async fn sync_lease_revoke_request(
        &self,
        id: &ProposeId,
        req: &LeaseRevokeRequest,
    ) -> Result<(), ExecuteError> {
        self.db.buffer_op(id, WriteOp::DeleteLease(req.id));
        let keys = match self.lease_collection.read().lease_map.get(&req.id) {
            Some(l) => l.keys(),
            None => return Err(ExecuteError::lease_not_found(req.id)),
        };

        if keys.is_empty() {
            let _ignore = self.lease_collection.write().revoke(req.id);
            return Ok(());
        }

        let revision = self.revision.next();
        let (prev_keys, del_revs): (Vec<Vec<u8>>, Vec<Revision>) = keys
            .into_iter()
            .zip(0..)
            .map(|(key, sub_revision)| {
                let (prev_rev, del_rev) = self
                    .index
                    .delete(&key, &[], revision, sub_revision)
                    .pop()
                    .unwrap_or_else(|| panic!("delete one key should return 1 result"));
                (prev_rev.encode_to_vec(), del_rev)
            })
            .unzip();
        let prev_kvs: Vec<KeyValue> = self
            .db
            .get_values(KV_TABLE, &prev_keys)?
            .into_iter()
            .flatten()
            .map(|v| KeyValue::decode(v.as_slice()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                ExecuteError::DbError(format!("Failed to decode key-value from DB, error: {e}"))
            })?;
        assert_eq!(prev_kvs.len(), del_revs.len());
        for kv in &prev_kvs {
            let lease_id = self.get_lease(&kv.key);
            self.detach(lease_id, kv.key.as_slice())?;
        }
        prev_kvs
            .iter()
            .zip(del_revs.into_iter())
            .for_each(|(kv, del_rev)| {
                let del_kv = KeyValue {
                    key: kv.key.clone(),
                    mod_revision: del_rev.revision(),
                    ..KeyValue::default()
                };
                self.db
                    .buffer_op(id, WriteOp::PutKeyValue(del_rev, del_kv.encode_to_vec()));
            });

        let updates = prev_kvs
            .into_iter()
            .map(|prev| {
                let kv = KeyValue {
                    key: prev.key.clone(),
                    mod_revision: revision,
                    ..Default::default()
                };
                Event {
                    #[allow(clippy::as_conversions)] // This cast is always valid
                    r#type: EventType::Delete as i32,
                    kv: Some(kv),
                    prev_kv: Some(prev),
                }
            })
            .collect();

        let _ignore = self.lease_collection.write().revoke(req.id);
        assert!(
            self.kv_update_tx.send((revision, updates)).await.is_ok(),
            "Failed to send updates to KV watcher"
        );
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::{error::Error, time::Duration};

    use utils::config::StorageConfig;

    use super::*;
    use crate::storage::db::DBProxy;

    #[tokio::test(flavor = "multi_thread", worker_threads = 10)]
    async fn test_lease_storage() -> Result<(), Box<dyn Error>> {
        let db = DBProxy::open(&StorageConfig::Memory)?;
        let lease_store = init_store(db);

        let req1 = RequestWithToken::new(LeaseGrantRequest { ttl: 10, id: 1 }.into());
        let _ignore1 = exe_and_sync_req(&lease_store, &req1).await?;

        let lo = lease_store.look_up(1).unwrap();
        assert_eq!(lo.id(), 1);
        assert_eq!(lo.ttl(), Duration::from_secs(10));
        assert_eq!(lease_store.leases().len(), 1);

        let attach_non_existing_lease = lease_store.inner.attach(0, "key".into());
        assert!(attach_non_existing_lease.is_err());
        let attach_existing_lease = lease_store.inner.attach(1, "key".into());
        assert!(attach_existing_lease.is_ok());
        lease_store.inner.detach(1, "key".as_bytes())?;

        let req2 = RequestWithToken::new(LeaseRevokeRequest { id: 1 }.into());
        let _ignore2 = exe_and_sync_req(&lease_store, &req2).await?;
        assert!(lease_store.look_up(1).is_none());
        assert!(lease_store.leases().is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_recover() -> Result<(), ExecuteError> {
        let db = DBProxy::open(&StorageConfig::Memory)?;
        let store = init_store(Arc::clone(&db));

        let req1 = RequestWithToken::new(LeaseGrantRequest { ttl: 10, id: 1 }.into());
        let _ignore1 = exe_and_sync_req(&store, &req1).await?;
        store.inner.attach(1, "key".into())?;

        let new_store = init_store(db);
        assert!(new_store.look_up(1).is_none());
        new_store.inner.recover_from_current_db()?;

        let lease1 = store.look_up(1).unwrap();
        let lease2 = new_store.look_up(1).unwrap();

        assert_eq!(lease1.id(), lease2.id());
        assert_eq!(lease1.ttl(), lease2.ttl());
        assert!(!lease1.keys().is_empty());
        assert!(lease2.keys().is_empty()); // keys will be recovered when recover kv store

        Ok(())
    }

    fn init_store(db: Arc<DBProxy>) -> LeaseStore<DBProxy> {
        let (_, lease_cmd_rx) = mpsc::channel(1);
        let (kv_update_tx, _) = mpsc::channel(1);
        let state = Arc::new(State::default());
        let header_gen = Arc::new(HeaderGenerator::new(0, 0));
        let index = Arc::new(Index::new());
        LeaseStore::new(lease_cmd_rx, state, header_gen, db, index, kv_update_tx)
    }

    async fn exe_and_sync_req(
        ls: &LeaseStore<DBProxy>,
        req: &RequestWithToken,
    ) -> Result<ResponseWrapper, ExecuteError> {
        let cmd_res = ls.execute(req)?;
        let id = ProposeId::new("test-id".to_owned());
        let _ignore = ls.after_sync(&id, req).await?;
        ls.inner.db.flush(&id)?;
        Ok(cmd_res.decode())
    }
}

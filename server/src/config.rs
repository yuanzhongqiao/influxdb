use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
};

use data_types::{database_rules::DatabaseRules, server_id::ServerId, DatabaseName};
use metrics::MetricRegistry;
use object_store::{path::ObjectStorePath, ObjectStore};
use parquet_file::catalog::PreservedCatalog;
use query::exec::Executor;

/// This module contains code for managing the configuration of the server.
use crate::{
    db::{catalog::Catalog, Db},
    write_buffer::KafkaBuffer,
    Error, JobRegistry, Result,
};
use observability_deps::tracing::{self, error, info, warn, Instrument};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(crate) const DB_RULES_FILE_NAME: &str = "rules.pb";

/// The Config tracks the configuration of databases and their rules along
/// with host groups for replication. It is used as an in-memory structure
/// that can be loaded incrementally from object storage.
///
/// drain() should be called prior to drop to ensure termination
/// of background worker tasks. They will be cancelled on drop
/// but they are effectively "detached" at that point, and they may not
/// run to completion if the tokio runtime is dropped
#[derive(Debug)]
pub(crate) struct Config {
    shutdown: CancellationToken,
    jobs: Arc<JobRegistry>,
    state: RwLock<ConfigState>,
    metric_registry: Arc<MetricRegistry>,
}

pub(crate) enum UpdateError<E> {
    Update(Error),
    Closure(E),
}

impl<E> From<Error> for UpdateError<E> {
    fn from(e: Error) -> Self {
        Self::Update(e)
    }
}

impl Config {
    pub(crate) fn new(
        jobs: Arc<JobRegistry>,
        metric_registry: Arc<MetricRegistry>,
        remote_template: Option<RemoteTemplate>,
    ) -> Self {
        Self {
            shutdown: Default::default(),
            state: RwLock::new(ConfigState::new(remote_template)),
            jobs,
            metric_registry,
        }
    }

    pub(crate) fn create_db(
        &self,
        db_name: DatabaseName<'static>,
    ) -> Result<CreateDatabaseHandle<'_>> {
        let mut state = self.state.write().expect("mutex poisoned");
        if state.reservations.contains(&db_name)
            || state.databases.contains_key(&db_name)
            || state.uninitialized_databases.contains_key(&db_name)
        {
            return Err(Error::DatabaseAlreadyExists {
                db_name: db_name.to_string(),
            });
        }

        state.reservations.insert(db_name.clone());
        Ok(CreateDatabaseHandle {
            db_name: Some(db_name),
            config: &self,
        })
    }

    pub(crate) fn recover_db(
        &self,
        db_name: DatabaseName<'static>,
    ) -> Result<RecoverDatabaseHandle<'_>> {
        let mut state = self.state.write().expect("mutex poisoned");
        if state.reservations.contains(&db_name) || state.databases.contains_key(&db_name) {
            return Err(Error::DatabaseAlreadyExists {
                db_name: db_name.to_string(),
            });
        }
        let rules = state
            .uninitialized_databases
            .get(&db_name)
            .cloned()
            .ok_or_else(|| Error::DatabaseNotFound {
                db_name: db_name.to_string(),
            })?;

        state.reservations.insert(db_name.clone());
        Ok(RecoverDatabaseHandle {
            db_name: Some(db_name),
            rules,
            config: &self,
        })
    }

    pub(crate) fn block_db(
        &self,
        db_name: DatabaseName<'static>,
    ) -> Result<BlockDatabaseHandle<'_>> {
        let mut state = self.state.write().expect("mutex poisoned");
        if state.reservations.contains(&db_name)
            || state.databases.contains_key(&db_name)
            || state.uninitialized_databases.contains_key(&db_name)
        {
            return Err(Error::DatabaseAlreadyExists {
                db_name: db_name.to_string(),
            });
        }

        state.reservations.insert(db_name.clone());
        Ok(BlockDatabaseHandle {
            db_name: Some(db_name),
            config: &self,
        })
    }

    pub(crate) fn db(&self, name: &DatabaseName<'_>) -> Option<Arc<Db>> {
        let state = self.state.read().expect("mutex poisoned");
        state.databases.get(name).map(|x| Arc::clone(&x.db))
    }

    pub(crate) fn has_uninitialized_database(&self, name: &DatabaseName<'_>) -> bool {
        let state = self.state.read().expect("mutex poisoned");
        state.uninitialized_databases.contains_key(name)
    }

    pub(crate) fn db_names_sorted(&self) -> Vec<DatabaseName<'static>> {
        let state = self.state.read().expect("mutex poisoned");
        let mut names: Vec<_> = state
            .reservations
            .iter()
            .cloned()
            .chain(state.databases.keys().cloned())
            .chain(state.uninitialized_databases.keys().cloned())
            .collect();
        names.sort();
        names
    }

    pub(crate) fn update_db_rules<F, E>(
        &self,
        db_name: &DatabaseName<'static>,
        update: F,
    ) -> std::result::Result<DatabaseRules, UpdateError<E>>
    where
        F: FnOnce(DatabaseRules) -> std::result::Result<DatabaseRules, E>,
    {
        let state = self.state.read().expect("mutex poisoned");
        let db_state = state
            .databases
            .get(db_name)
            .ok_or_else(|| Error::DatabaseNotFound {
                db_name: db_name.to_string(),
            })?;

        let mut rules = db_state.db.rules.write();
        *rules = update(rules.clone()).map_err(UpdateError::Closure)?;
        Ok(rules.clone())
    }

    pub(crate) fn remotes_sorted(&self) -> Vec<(ServerId, String)> {
        let state = self.state.read().expect("mutex poisoned");
        state.remotes.iter().map(|(&a, b)| (a, b.clone())).collect()
    }

    pub(crate) fn update_remote(&self, id: ServerId, addr: GRpcConnectionString) {
        let mut state = self.state.write().expect("mutex poisoned");
        state.remotes.insert(id, addr);
    }

    pub(crate) fn delete_remote(&self, id: ServerId) -> Option<GRpcConnectionString> {
        let mut state = self.state.write().expect("mutex poisoned");
        state.remotes.remove(&id)
    }

    pub(crate) fn resolve_remote(&self, id: ServerId) -> Option<GRpcConnectionString> {
        let state = self.state.read().expect("mutex poisoned");
        state
            .remotes
            .get(&id)
            .cloned()
            .or_else(|| state.remote_template.as_ref().map(|t| t.get(&id)))
    }

    fn commit_db(
        &self,
        rules: DatabaseRules,
        server_id: ServerId,
        object_store: Arc<ObjectStore>,
        exec: Arc<Executor>,
        preserved_catalog: PreservedCatalog<Catalog>,
    ) {
        let mut state = self.state.write().expect("mutex poisoned");
        let name = rules.name.clone();

        if self.shutdown.is_cancelled() {
            error!("server is shutting down");
            return;
        }

        // Right now, `KafkaBuffer` is the only production implementation of the `WriteBuffer`
        // trait, so always use `KafkaBuffer` when there is a write buffer connection string
        // specified. If/when there are other kinds of write buffers, additional configuration will
        // be needed to determine what kind of write buffer to use here.
        let write_buffer = rules
            .write_buffer_connection_string
            .as_ref()
            .map(|conn| Arc::new(KafkaBuffer::new(conn)) as _);

        let db = Arc::new(Db::new(
            rules,
            server_id,
            object_store,
            exec,
            Arc::clone(&self.jobs),
            preserved_catalog,
            write_buffer,
        ));

        let shutdown = self.shutdown.child_token();
        let shutdown_captured = shutdown.clone();
        let db_captured = Arc::clone(&db);
        let name_captured = name.clone();

        let handle = Some(tokio::spawn(async move {
            db_captured
                .background_worker(shutdown_captured)
                .instrument(tracing::info_span!("db_worker", database=%name_captured))
                .await
        }));

        assert!(state
            .databases
            .insert(
                name.clone(),
                DatabaseState {
                    db,
                    handle,
                    shutdown
                }
            )
            .is_none());
        state.reservations.remove(&name);
        state.uninitialized_databases.remove(&name);
    }

    fn commit_uninitialized(&self, name: DatabaseName<'static>, rules: Option<DatabaseRules>) {
        let mut state = self.state.write().expect("mutex poisoned");

        assert!(state
            .uninitialized_databases
            .insert(name.clone(), rules,)
            .is_none());
        state.reservations.remove(&name);
    }

    fn rollback(&self, name: &DatabaseName<'static>) {
        let mut state = self.state.write().expect("mutex poisoned");
        state.reservations.remove(name);
    }

    /// Cancels and drains all background worker tasks
    pub(crate) async fn drain(&self) {
        info!("shutting down database background workers");

        // This will cancel all background child tasks
        self.shutdown.cancel();

        let handles: Vec<_> = self
            .state
            .write()
            .expect("mutex poisoned")
            .databases
            .iter_mut()
            .filter_map(|(_, v)| v.join())
            .collect();

        for handle in handles {
            let _ = handle.await;
        }

        info!("database background workers shutdown");
    }

    pub fn metrics_registry(&self) -> Arc<MetricRegistry> {
        Arc::clone(&self.metric_registry)
    }
}

pub fn object_store_path_for_database_config<P: ObjectStorePath>(
    root: &P,
    name: &DatabaseName<'_>,
) -> P {
    let mut path = root.clone();
    path.push_dir(name.to_string());
    path.set_file_name(DB_RULES_FILE_NAME);
    path
}

/// A gRPC connection string.
pub type GRpcConnectionString = String;

#[derive(Default, Debug)]
struct ConfigState {
    /// Databases that are known (but not exist due to init errors).
    reservations: BTreeSet<DatabaseName<'static>>,

    /// Rules for databases that are reserved but that could not be initialized.
    uninitialized_databases: BTreeMap<DatabaseName<'static>, Option<DatabaseRules>>,

    /// Databases that work.
    databases: BTreeMap<DatabaseName<'static>, DatabaseState>,

    /// Map between remote IOx server IDs and management API connection strings.
    remotes: BTreeMap<ServerId, GRpcConnectionString>,

    /// Static map between remote server IDs and hostnames based on a template
    remote_template: Option<RemoteTemplate>,
}

impl ConfigState {
    fn new(remote_template: Option<RemoteTemplate>) -> Self {
        Self {
            remote_template,
            ..Default::default()
        }
    }
}

/// A RemoteTemplate string is a remote connection template string.
/// Occurrences of the substring "{id}" in the template will be replaced
/// by the server ID.
#[derive(Debug)]
pub struct RemoteTemplate {
    template: String,
}

impl RemoteTemplate {
    pub fn new(template: impl Into<String>) -> Self {
        let template = template.into();
        Self { template }
    }

    fn get(&self, id: &ServerId) -> GRpcConnectionString {
        self.template.replace("{id}", &format!("{}", id.get_u32()))
    }
}

#[derive(Debug)]
struct DatabaseState {
    db: Arc<Db>,
    handle: Option<JoinHandle<()>>,
    shutdown: CancellationToken,
}

impl DatabaseState {
    fn join(&mut self) -> Option<JoinHandle<()>> {
        self.handle.take()
    }
}

impl Drop for DatabaseState {
    fn drop(&mut self) {
        if self.handle.is_some() {
            // Join should be called on `DatabaseState` prior to dropping, for example, by
            // calling drain() on the owning `Config`
            warn!("DatabaseState dropped without waiting for background task to complete");
            self.shutdown.cancel();
        }
    }
}

/// CreateDatabaseHandle is returned when a call is made to `create_db` on
/// the Config struct. The handle can be used to hold a reservation for the
/// database name. Calling `commit` on the handle will consume the struct
/// and move the database from reserved to being in the config.
///
/// The goal is to ensure that database names can be reserved with
/// minimal time holding a write lock on the config state. This allows
/// the caller (the server) to reserve the database name, persist its
/// configuration and then commit the change in-memory after it has been
/// persisted.
#[derive(Debug)]
pub(crate) struct CreateDatabaseHandle<'a> {
    /// Partial moves aren't supported on structures that implement Drop
    /// so use Option to allow taking DatabaseRules out in `commit`
    db_name: Option<DatabaseName<'static>>,
    config: &'a Config,
}

impl<'a> CreateDatabaseHandle<'a> {
    pub(crate) fn commit_db(
        mut self,
        server_id: ServerId,
        object_store: Arc<ObjectStore>,
        exec: Arc<Executor>,
        preserved_catalog: PreservedCatalog<Catalog>,
        rules: DatabaseRules,
    ) -> Result<()> {
        let db_name = self.db_name.take().expect("not committed");
        if db_name != rules.name {
            self.config.rollback(&db_name);
            return Err(Error::RulesDatabaseNameMismatch {
                actual: rules.name.to_string(),
                expected: db_name.to_string(),
            });
        }

        self.config
            .commit_db(rules, server_id, object_store, exec, preserved_catalog);

        Ok(())
    }

    pub(crate) fn commit_rules_only(mut self, rules: DatabaseRules) -> Result<()> {
        let db_name = self.db_name.take().expect("not committed");
        if db_name != rules.name {
            self.config.rollback(&db_name);
            return Err(Error::RulesDatabaseNameMismatch {
                actual: rules.name.to_string(),
                expected: db_name.to_string(),
            });
        }

        self.config.commit_uninitialized(db_name, Some(rules));

        Ok(())
    }

    pub(crate) fn commit_no_rules(mut self) {
        let db_name = self.db_name.take().expect("not committed");

        self.config.commit_uninitialized(db_name, None);
    }
}

impl<'a> Drop for CreateDatabaseHandle<'a> {
    fn drop(&mut self) {
        if let Some(db_name) = self.db_name.take() {
            self.config.rollback(&db_name)
        }
    }
}

#[derive(Debug)]
pub(crate) struct RecoverDatabaseHandle<'a> {
    /// Partial moves aren't supported on structures that implement Drop
    /// so use Option to allow taking DatabaseRules out in `commit`
    db_name: Option<DatabaseName<'static>>,
    rules: Option<DatabaseRules>,
    config: &'a Config,
}

impl<'a> RecoverDatabaseHandle<'a> {
    pub(crate) fn commit_db(
        mut self,
        server_id: ServerId,
        object_store: Arc<ObjectStore>,
        exec: Arc<Executor>,
        preserved_catalog: PreservedCatalog<Catalog>,
        rules: Option<DatabaseRules>,
    ) -> Result<()> {
        let db_name = self.db_name.take().expect("not committed");
        let rules = rules
            .or_else(|| self.rules.take())
            .ok_or_else(|| Error::NoRulesLoaded {
                db_name: db_name.to_string(),
            })?;
        if db_name != rules.name {
            self.config.rollback(&db_name);
            return Err(Error::RulesDatabaseNameMismatch {
                actual: rules.name.to_string(),
                expected: db_name.to_string(),
            });
        }

        self.config
            .commit_db(rules, server_id, object_store, exec, preserved_catalog);

        Ok(())
    }

    pub(crate) fn has_rules(&self) -> bool {
        self.rules.is_some()
    }

    pub(crate) fn abort(mut self) {
        let db_name = self.db_name.take().expect("not committed");

        self.config.rollback(&db_name);
    }
}

impl<'a> Drop for RecoverDatabaseHandle<'a> {
    fn drop(&mut self) {
        if let Some(db_name) = self.db_name.take() {
            self.config.rollback(&db_name)
        }
    }
}

/// Handle that blocks DB creations. Useful when messing with the preserved catalog of unregistered DBs.
#[derive(Debug)]
pub(crate) struct BlockDatabaseHandle<'a> {
    /// Partial moves aren't supported on structures that implement Drop
    /// so use Option to allow taking DatabaseRules out in `commit`
    db_name: Option<DatabaseName<'static>>,
    config: &'a Config,
}

impl<'a> Drop for BlockDatabaseHandle<'a> {
    fn drop(&mut self) {
        if let Some(db_name) = self.db_name.take() {
            self.config.rollback(&db_name)
        }
    }
}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use object_store::{memory::InMemory, ObjectStore, ObjectStoreApi};

    use crate::db::load_or_create_preserved_catalog;

    use super::*;
    use std::num::NonZeroU32;

    #[tokio::test]
    async fn create_db() {
        let name = DatabaseName::new("foo").unwrap();
        let metric_registry = Arc::new(metrics::MetricRegistry::new());
        let config = Config::new(
            Arc::new(JobRegistry::new()),
            Arc::clone(&metric_registry),
            None,
        );

        {
            let _db_reservation = config.create_db(name.clone()).unwrap();
            let err = config.create_db(name.clone()).unwrap_err();
            assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));
        }

        let server_id = ServerId::try_from(1).unwrap();
        let store = Arc::new(ObjectStore::new_in_memory(InMemory::new()));
        let exec = Arc::new(Executor::new(1));
        let preserved_catalog = load_or_create_preserved_catalog(
            &name,
            Arc::clone(&store),
            server_id,
            config.metrics_registry(),
            false,
        )
        .await
        .unwrap();
        let rules = DatabaseRules::new(name.clone());

        {
            let db_reservation = config.create_db(DatabaseName::new("bar").unwrap()).unwrap();
            let err = db_reservation
                .commit_db(
                    server_id,
                    Arc::clone(&store),
                    Arc::clone(&exec),
                    preserved_catalog,
                    rules.clone(),
                )
                .unwrap_err();
            assert!(matches!(err, Error::RulesDatabaseNameMismatch { .. }));
        }

        let preserved_catalog = load_or_create_preserved_catalog(
            &name,
            Arc::clone(&store),
            server_id,
            config.metrics_registry(),
            false,
        )
        .await
        .unwrap();
        let db_reservation = config.create_db(name.clone()).unwrap();
        db_reservation
            .commit_db(server_id, store, exec, preserved_catalog, rules)
            .unwrap();
        assert!(config.db(&name).is_some());
        assert_eq!(config.db_names_sorted(), vec![name.clone()]);

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        assert!(
            config
                .db(&name)
                .expect("expected database")
                .worker_iterations_lifecycle()
                > 0
        );
        assert!(
            config
                .db(&name)
                .expect("expected database")
                .worker_iterations_cleanup()
                > 0
        );

        let err = config.recover_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        let err = config.create_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        let err = config.block_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        config.drain().await
    }

    #[tokio::test]
    async fn recover_db() {
        let name = DatabaseName::new("foo").unwrap();
        let metric_registry = Arc::new(metrics::MetricRegistry::new());
        let config = Config::new(
            Arc::new(JobRegistry::new()),
            Arc::clone(&metric_registry),
            None,
        );

        {
            let db_reservation = config.create_db(name.clone()).unwrap();

            let err = db_reservation
                .commit_rules_only(DatabaseRules::new(DatabaseName::new("bar").unwrap()))
                .unwrap_err();
            assert!(matches!(err, Error::RulesDatabaseNameMismatch { .. }));
        }

        assert!(!config.has_uninitialized_database(&name));

        let rules = DatabaseRules::new(name.clone());
        {
            let db_reservation = config.create_db(name.clone()).unwrap();
            db_reservation.commit_rules_only(rules).unwrap();
        }

        assert!(config.has_uninitialized_database(&name));

        let err = config.create_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        let err = config
            .recover_db(DatabaseName::new("bar").unwrap())
            .unwrap_err();
        assert!(matches!(err, Error::DatabaseNotFound { .. }));

        let server_id = ServerId::try_from(1).unwrap();
        let store = Arc::new(ObjectStore::new_in_memory(InMemory::new()));
        let exec = Arc::new(Executor::new(1));
        let preserved_catalog = load_or_create_preserved_catalog(
            &name,
            Arc::clone(&store),
            server_id,
            config.metrics_registry(),
            false,
        )
        .await
        .unwrap();
        {
            let db_reservation = config.recover_db(name.clone()).unwrap();
            let err = db_reservation
                .commit_db(
                    server_id,
                    Arc::clone(&store),
                    Arc::clone(&exec),
                    preserved_catalog,
                    Some(DatabaseRules::new(DatabaseName::new("bar").unwrap())),
                )
                .unwrap_err();
            assert!(matches!(err, Error::RulesDatabaseNameMismatch { .. }));
        }

        let preserved_catalog = load_or_create_preserved_catalog(
            &name,
            Arc::clone(&store),
            server_id,
            config.metrics_registry(),
            false,
        )
        .await
        .unwrap();
        let db_reservation = config.recover_db(name.clone()).unwrap();
        assert!(db_reservation.has_rules());
        db_reservation
            .commit_db(server_id, store, exec, preserved_catalog, None)
            .unwrap();
        assert!(config.db(&name).is_some());
        assert_eq!(config.db_names_sorted(), vec![name.clone()]);
        assert!(!config.has_uninitialized_database(&name));

        let err = config.recover_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        let err = config.create_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        let err = config.block_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        config.drain().await
    }

    #[tokio::test]
    async fn block_db() {
        let name = DatabaseName::new("foo").unwrap();
        let metric_registry = Arc::new(metrics::MetricRegistry::new());
        let config = Config::new(
            Arc::new(JobRegistry::new()),
            Arc::clone(&metric_registry),
            None,
        );

        let handle = config.block_db(name.clone()).unwrap();

        let err = config.recover_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        let err = config.create_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        let err = config.block_db(name.clone()).unwrap_err();
        assert!(matches!(err, Error::DatabaseAlreadyExists { .. }));

        drop(handle);

        config.create_db(name.clone()).unwrap();

        config.drain().await
    }

    #[tokio::test]
    async fn test_db_drop() {
        let name = DatabaseName::new("foo").unwrap();
        let metric_registry = Arc::new(metrics::MetricRegistry::new());
        let config = Config::new(
            Arc::new(JobRegistry::new()),
            Arc::clone(&metric_registry),
            None,
        );
        let rules = DatabaseRules::new(name.clone());

        let db_reservation = config.create_db(name.clone()).unwrap();
        let server_id = ServerId::try_from(1).unwrap();
        let store = Arc::new(ObjectStore::new_in_memory(InMemory::new()));
        let exec = Arc::new(Executor::new(1));
        let preserved_catalog = load_or_create_preserved_catalog(
            &name,
            Arc::clone(&store),
            server_id,
            config.metrics_registry(),
            false,
        )
        .await
        .unwrap();
        db_reservation
            .commit_db(server_id, store, exec, preserved_catalog, rules)
            .unwrap();

        let token = config
            .state
            .read()
            .expect("lock poisoned")
            .databases
            .get(&name)
            .unwrap()
            .shutdown
            .clone();

        // Drop config without calling drain
        std::mem::drop(config);

        // This should cancel the the background task
        assert!(token.is_cancelled());
    }

    #[test]
    fn object_store_path_for_database_config() {
        let storage = ObjectStore::new_in_memory(InMemory::new());
        let mut base_path = storage.new_path();
        base_path.push_dir("1");

        let name = DatabaseName::new("foo").unwrap();
        let rules_path = super::object_store_path_for_database_config(&base_path, &name);

        let mut expected_path = base_path;
        expected_path.push_dir("foo");
        expected_path.set_file_name("rules.pb");

        assert_eq!(rules_path, expected_path);
    }

    #[test]
    fn resolve_remote() {
        let metric_registry = Arc::new(metrics::MetricRegistry::new());
        let config = Config::new(
            Arc::new(JobRegistry::new()),
            Arc::clone(&metric_registry),
            Some(RemoteTemplate::new("http://iox-query-{id}:8082")),
        );

        let server_id = ServerId::new(NonZeroU32::new(42).unwrap());
        let remote = config.resolve_remote(server_id);
        assert_eq!(
            remote,
            Some(GRpcConnectionString::from("http://iox-query-42:8082"))
        );

        let server_id = ServerId::new(NonZeroU32::new(24).unwrap());
        let remote = config.resolve_remote(server_id);
        assert_eq!(
            remote,
            Some(GRpcConnectionString::from("http://iox-query-24:8082"))
        );
    }
}

use futures::stream::poll_fn;
use futures::{Async, Future, Poll, Stream};
use lazy_static::lazy_static;
use mockall::predicate::*;
use mockall::*;
use serde::{Deserialize, Serialize};
use stable_hash::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use web3::types::{Address, H256};

use crate::data::subgraph::status;
use crate::data::{query::QueryTarget, subgraph::schema::*};
use crate::data::{store::*, subgraph::Source};
use crate::prelude::*;
use crate::util::lfu_cache::LfuCache;

use crate::components::server::index_node::VersionInfo;

lazy_static! {
    pub static ref SUBSCRIPTION_THROTTLE_INTERVAL: Duration =
        env::var("SUBSCRIPTION_THROTTLE_INTERVAL")
            .ok()
            .map(|s| u64::from_str(&s).unwrap_or_else(|_| panic!(
                "failed to parse env var SUBSCRIPTION_THROTTLE_INTERVAL"
            )))
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(1000));
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntityType {
    Data(String),
    Metadata(MetadataType),
}

impl EntityType {
    pub fn data(entity_type: String) -> Self {
        Self::Data(entity_type)
    }

    pub fn metadata(entity_type: MetadataType) -> Self {
        Self::Metadata(entity_type)
    }

    pub fn is_data_type(&self) -> bool {
        match self {
            Self::Data(_) => true,
            Self::Metadata(_) => false,
        }
    }

    pub fn is_data(&self, entity_type: &str) -> bool {
        match self {
            Self::Data(s) => s == entity_type,
            Self::Metadata(_) => false,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Data(name) => name.as_str(),
            Self::Metadata(typ) => typ.as_str(),
        }
    }

    /// Expect `self` to reference a `Data` entity type and return the
    /// type's name
    ///
    /// # Panics
    /// This method will panic if `self` is a `Metadata` type
    pub fn expect_data(&self) -> &str {
        match self {
            Self::Data(s) => s.as_str(),
            Self::Metadata(_) => {
                unreachable!("callers check that this is never called for metadata")
            }
        }
    }

    /// Expect `self` to reference a `Metadata` entity type and return the
    /// type's name
    ///
    /// # Panics
    /// This method will panic if `self` is a `Data` type
    pub fn expect_metadata(&self) -> &str {
        match self {
            Self::Data(_) => unreachable!("callers check that this is never called for data"),
            Self::Metadata(typ) => typ.as_str(),
        }
    }
}

impl fmt::Display for EntityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data(s) => write!(f, "{}", s),
            Self::Metadata(typ) => write!(f, "%{}", typ.as_str()),
        }
    }
}

impl From<MetadataType> for EntityType {
    fn from(typ: MetadataType) -> Self {
        EntityType::Metadata(typ)
    }
}

// Note: Do not modify fields without making a backward compatible change to
// the StableHash impl (below)
/// Key by which an individual entity in the store can be accessed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityKey {
    /// ID of the subgraph.
    pub subgraph_id: SubgraphDeploymentId,

    /// Name of the entity type.
    pub entity_type: EntityType,

    /// ID of the individual entity.
    pub entity_id: String,
}

impl StableHash for EntityKey {
    fn stable_hash<H: StableHasher>(&self, mut sequence_number: H::Seq, state: &mut H) {
        use EntityType::*;

        match &self.entity_type {
            Data(name) => {
                // EntityType used to just be a string for normal entities
                self.subgraph_id
                    .stable_hash(sequence_number.next_child(), state);
                name.stable_hash(sequence_number.next_child(), state);
                self.entity_id
                    .stable_hash(sequence_number.next_child(), state);
            }
            Metadata(typ) => {
                // We used to represent metadata entities with
                // SubgraphDeploymentId("subgraphs") and the entity type was just
                // the string representation of MetadataType.
                let name = typ.to_string();
                "subgraphs".stable_hash(sequence_number.next_child().next_child(), state);
                name.stable_hash(sequence_number.next_child(), state);
                self.entity_id
                    .stable_hash(sequence_number.next_child(), state);
            }
        }
    }
}

impl EntityKey {
    pub fn data(subgraph_id: SubgraphDeploymentId, entity_type: String, entity_id: String) -> Self {
        Self {
            subgraph_id,
            entity_type: EntityType::Data(entity_type),
            entity_id,
        }
    }

    pub fn metadata(
        subgraph_id: SubgraphDeploymentId,
        entity_type: MetadataType,
        entity_id: String,
    ) -> Self {
        Self {
            subgraph_id,
            entity_type: EntityType::Metadata(entity_type),
            entity_id,
        }
    }
}

#[test]
fn key_stable_hash() {
    use stable_hash::crypto::SetHasher;
    use stable_hash::utils::stable_hash;

    #[track_caller]
    fn hashes_to(key: &EntityKey, exp: &str) {
        let hash = hex::encode(stable_hash::<SetHasher, _>(&key));
        assert_eq!(exp, hash.as_str());
    }

    let id = SubgraphDeploymentId::new("QmP9MRvVzwHxr3sGvujihbvJzcTz2LYLMfi5DyihBg6VUd").unwrap();
    let key = EntityKey::data(id.clone(), "Account".to_string(), "0xdeadbeef".to_string());
    hashes_to(
        &key,
        "905b57035d6f98cff8281e7b055e10570a2bd31190507341c6716af2d3c1ad98",
    );

    let key = EntityKey::metadata(
        id.clone(),
        MetadataType::DynamicEthereumContractDataSource,
        format!("{}-manifest-data-source-1", &id),
    );
    hashes_to(
        &key,
        "062283bacf94a7910f1332889e0a11f4abce34fa666eb883aabbcd248e8be1c3",
    );
}

/// Supported types of store filters.
#[derive(Clone, Debug, PartialEq)]
pub enum EntityFilter {
    And(Vec<EntityFilter>),
    Or(Vec<EntityFilter>),
    Equal(Attribute, Value),
    Not(Attribute, Value),
    GreaterThan(Attribute, Value),
    LessThan(Attribute, Value),
    GreaterOrEqual(Attribute, Value),
    LessOrEqual(Attribute, Value),
    In(Attribute, Vec<Value>),
    NotIn(Attribute, Vec<Value>),
    Contains(Attribute, Value),
    NotContains(Attribute, Value),
    StartsWith(Attribute, Value),
    NotStartsWith(Attribute, Value),
    EndsWith(Attribute, Value),
    NotEndsWith(Attribute, Value),
}

// Define some convenience methods
impl EntityFilter {
    pub fn new_equal(
        attribute_name: impl Into<Attribute>,
        attribute_value: impl Into<Value>,
    ) -> Self {
        EntityFilter::Equal(attribute_name.into(), attribute_value.into())
    }

    pub fn new_in(
        attribute_name: impl Into<Attribute>,
        attribute_values: Vec<impl Into<Value>>,
    ) -> Self {
        EntityFilter::In(
            attribute_name.into(),
            attribute_values.into_iter().map(Into::into).collect(),
        )
    }

    pub fn and_maybe(self, other: Option<Self>) -> Self {
        use EntityFilter as f;
        match other {
            Some(other) => match (self, other) {
                (f::And(mut fs1), f::And(mut fs2)) => {
                    fs1.append(&mut fs2);
                    f::And(fs1)
                }
                (f::And(mut fs1), f2) => {
                    fs1.push(f2);
                    f::And(fs1)
                }
                (f1, f::And(mut fs2)) => {
                    fs2.push(f1);
                    f::And(fs2)
                }
                (f1, f2) => f::And(vec![f1, f2]),
            },
            None => self,
        }
    }
}

/// The order in which entities should be restored from a store.
#[derive(Clone, Debug, PartialEq)]
pub enum EntityOrder {
    /// Order ascending by the given attribute. Use `id` as a tie-breaker
    Ascending(String, ValueType),
    /// Order descending by the given attribute. Use `id` as a tie-breaker
    Descending(String, ValueType),
    /// Order by the `id` of the entities
    Default,
    /// Do not order at all. This speeds up queries where we know that
    /// order does not matter
    Unordered,
}

/// How many entities to return, how many to skip etc.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityRange {
    /// Limit on how many entities to return.
    pub first: Option<u32>,

    /// How many entities to skip.
    pub skip: u32,
}

impl EntityRange {
    /// Query for the first `n` entities.
    pub fn first(n: u32) -> Self {
        Self {
            first: Some(n),
            skip: 0,
        }
    }
}

/// The attribute we want to window by in an `EntityWindow`. We have to
/// distinguish between scalar and list attributes since we need to use
/// different queries for them, and the JSONB storage scheme can not
/// determine that by itself
#[derive(Clone, Debug, PartialEq)]
pub enum WindowAttribute {
    Scalar(String),
    List(String),
}

impl WindowAttribute {
    pub fn name(&self) -> &str {
        match self {
            WindowAttribute::Scalar(name) => name,
            WindowAttribute::List(name) => name,
        }
    }
}

/// How to connect children to their parent when the child table does not
/// store parent id's
#[derive(Clone, Debug, PartialEq)]
pub enum ParentLink {
    /// The parent stores a list of child ids. The ith entry in the outer
    /// vector contains the id of the children for `EntityWindow.ids[i]`
    List(Vec<Vec<String>>),
    /// The parent stores the id of one child. The ith entry in the
    /// vector contains the id of the child of the parent with id
    /// `EntityWindow.ids[i]`
    Scalar(Vec<String>),
}

/// How many children a parent can have when the child stores
/// the id of the parent
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChildMultiplicity {
    Single,
    Many,
}

/// How to select children for their parents depending on whether the
/// child stores parent ids (`Direct`) or the parent
/// stores child ids (`Parent`)
#[derive(Clone, Debug, PartialEq)]
pub enum EntityLink {
    /// The parent id is stored in this child attribute
    Direct(WindowAttribute, ChildMultiplicity),
    /// Join with the parents table to get at the parent id
    Parent(ParentLink),
}

/// Window results of an `EntityQuery` query along the parent's id:
/// the `order_by`, `order_direction`, and `range` of the query apply to
/// entities that belong to the same parent. Only entities that belong to
/// one of the parents listed in `ids` will be included in the query result.
///
/// Note that different windows can vary both by the entity type and id of
/// the children, but also by how to get from a child to its parent, i.e.,
/// it is possible that two windows access the same entity type, but look
/// at different attributes to connect to parent entities
#[derive(Clone, Debug, PartialEq)]
pub struct EntityWindow {
    /// The entity type for this window
    pub child_type: String,
    /// The ids of parents that should be considered for this window
    pub ids: Vec<String>,
    /// How to get the parent id
    pub link: EntityLink,
}

/// The base collections from which we are going to get entities for use in
/// `EntityQuery`; the result of the query comes from applying the query's
/// filter and order etc. to the entities described in this collection. For
/// a windowed collection order and range are applied to each individual
/// window
#[derive(Clone, Debug, PartialEq)]
pub enum EntityCollection {
    /// Use all entities of the given types
    All(Vec<String>),
    /// Use entities according to the windows. The set of entities that we
    /// apply order and range to is formed by taking all entities matching
    /// the window, and grouping them by the attribute of the window. Entities
    /// that have the same value in the `attribute` field of their window are
    /// grouped together. Note that it is possible to have one window for
    /// entity type `A` and attribute `a`, and another for entity type `B` and
    /// column `b`; they will be grouped by using `A.a` and `B.b` as the keys
    Window(Vec<EntityWindow>),
}
/// The type we use for block numbers. This has to be a signed integer type
/// since Postgres does not support unsigned integer types. But 2G ought to
/// be enough for everybody
pub type BlockNumber = i32;

pub const BLOCK_NUMBER_MAX: BlockNumber = std::i32::MAX;

/// A query for entities in a store.
///
/// Details of how query generation for `EntityQuery` works can be found
/// at https://github.com/graphprotocol/rfcs/blob/master/engineering-plans/0001-graphql-query-prefetching.md
#[derive(Clone, Debug)]
pub struct EntityQuery {
    /// ID of the subgraph.
    pub subgraph_id: SubgraphDeploymentId,

    /// The block height at which to execute the query. Set this to
    /// `BLOCK_NUMBER_MAX` to run the query at the latest available block.
    /// If the subgraph uses JSONB storage, anything but `BLOCK_NUMBER_MAX`
    /// will cause an error as JSONB storage does not support querying anything
    /// but the latest block
    pub block: BlockNumber,

    /// The names of the entity types being queried. The result is the union
    /// (with repetition) of the query for each entity.
    pub collection: EntityCollection,

    /// Filter to filter entities by.
    pub filter: Option<EntityFilter>,

    /// How to order the entities
    pub order: EntityOrder,

    /// A range to limit the size of the result.
    pub range: EntityRange,

    /// Optional logger for anything related to this query
    pub logger: Option<Logger>,

    pub query_id: Option<String>,

    _force_use_of_new: (),
}

impl EntityQuery {
    pub fn new(
        subgraph_id: SubgraphDeploymentId,
        block: BlockNumber,
        collection: EntityCollection,
    ) -> Self {
        EntityQuery {
            subgraph_id,
            block,
            collection,
            filter: None,
            order: EntityOrder::Default,
            range: EntityRange::first(100),
            logger: None,
            query_id: None,
            _force_use_of_new: (),
        }
    }

    pub fn filter(mut self, filter: EntityFilter) -> Self {
        self.filter = Some(filter);
        self
    }

    pub fn order(mut self, order: EntityOrder) -> Self {
        self.order = order;
        self
    }

    pub fn range(mut self, range: EntityRange) -> Self {
        self.range = range;
        self
    }

    pub fn first(mut self, first: u32) -> Self {
        self.range.first = Some(first);
        self
    }

    pub fn skip(mut self, skip: u32) -> Self {
        self.range.skip = skip;
        self
    }

    pub fn simplify(mut self) -> Self {
        // If there is one window, with one id, in a direct relation to the
        // entities, we can simplify the query by changing the filter and
        // getting rid of the window
        if let EntityCollection::Window(windows) = &self.collection {
            if windows.len() == 1 {
                let window = windows.first().expect("we just checked");
                if window.ids.len() == 1 {
                    let id = window.ids.first().expect("we just checked");
                    if let EntityLink::Direct(attribute, _) = &window.link {
                        let filter = match attribute {
                            WindowAttribute::Scalar(name) => {
                                EntityFilter::Equal(name.to_owned(), id.into())
                            }
                            WindowAttribute::List(name) => {
                                EntityFilter::Contains(name.to_owned(), Value::from(vec![id]))
                            }
                        };
                        self.filter = Some(filter.and_maybe(self.filter));
                        self.collection = EntityCollection::All(vec![window.child_type.to_owned()]);
                    }
                }
            }
        }
        self
    }
}

/// Operation types that lead to entity changes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum EntityChangeOperation {
    /// An entity was added or updated
    Set,
    /// An existing entity was removed.
    Removed,
}

/// Entity change events emitted by [Store](trait.Store.html) implementations.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct EntityChange {
    /// ID of the subgraph the changed entity belongs to.
    pub subgraph_id: SubgraphDeploymentId,
    /// Entity type name of the changed entity.
    pub entity_type: EntityType,
    /// ID of the changed entity.
    pub entity_id: String,
    /// Operation that caused the change.
    pub operation: EntityChangeOperation,
}

impl EntityChange {
    pub fn from_key(key: EntityKey, operation: EntityChangeOperation) -> Self {
        Self {
            subgraph_id: key.subgraph_id,
            entity_type: key.entity_type,
            entity_id: key.entity_id,
            operation,
        }
    }

    pub fn as_filter(&self) -> SubscriptionFilter {
        SubscriptionFilter::Entities(self.subgraph_id.clone(), self.entity_type.clone())
    }
}

impl From<MetadataOperation> for EntityChange {
    fn from(operation: MetadataOperation) -> Self {
        use self::MetadataOperation::*;
        match operation {
            Set { key, .. } => EntityChange::from_key(key.into(), EntityChangeOperation::Set),
            Remove { key, .. } => {
                EntityChange::from_key(key.into(), EntityChangeOperation::Removed)
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// The store emits `StoreEvents` to indicate that some entities have changed.
/// For block-related data, at most one `StoreEvent` is emitted for each block
/// that is processed. The `changes` vector contains the details of what changes
/// were made, and to which entity.
///
/// Since the 'subgraph of subgraphs' is special, and not directly related to
/// any specific blocks, `StoreEvents` for it are generated as soon as they are
/// written to the store.
pub struct StoreEvent {
    // The tag is only there to make it easier to track StoreEvents in the
    // logs as they flow through the system
    pub tag: usize,
    pub changes: HashSet<EntityChange>,
}

impl From<Vec<MetadataOperation>> for StoreEvent {
    fn from(operations: Vec<MetadataOperation>) -> Self {
        let changes: Vec<_> = operations.into_iter().map(EntityChange::from).collect();
        StoreEvent::new(changes)
    }
}

impl<'a> FromIterator<&'a EntityModification> for StoreEvent {
    fn from_iter<I: IntoIterator<Item = &'a EntityModification>>(mods: I) -> Self {
        let changes: Vec<_> = mods
            .into_iter()
            .map(|op| {
                use self::EntityModification::*;
                match op {
                    Insert { key, .. } | Overwrite { key, .. } => {
                        EntityChange::from_key(key.clone(), EntityChangeOperation::Set)
                    }
                    Remove { key } => {
                        EntityChange::from_key(key.clone(), EntityChangeOperation::Removed)
                    }
                }
            })
            .collect();
        StoreEvent::new(changes)
    }
}

impl StoreEvent {
    pub fn new(changes: Vec<EntityChange>) -> StoreEvent {
        static NEXT_TAG: AtomicUsize = AtomicUsize::new(0);

        let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
        let changes = changes.into_iter().collect();
        StoreEvent { tag, changes }
    }

    /// Extend `ev1` with `ev2`. If `ev1` is `None`, just set it to `ev2`
    fn accumulate(logger: &Logger, ev1: &mut Option<StoreEvent>, ev2: StoreEvent) {
        if let Some(e) = ev1 {
            trace!(logger, "Adding changes to event";
                           "from" => ev2.tag, "to" => e.tag);
            e.changes.extend(ev2.changes);
        } else {
            *ev1 = Some(ev2);
        }
    }

    pub fn extend(mut self, other: StoreEvent) -> Self {
        self.changes.extend(other.changes);
        self
    }
}

impl fmt::Display for StoreEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "StoreEvent[{}](changes: {})",
            self.tag,
            self.changes.len()
        )
    }
}

impl PartialEq for StoreEvent {
    fn eq(&self, other: &StoreEvent) -> bool {
        // Ignore tag for equality
        self.changes == other.changes
    }
}

/// A `StoreEventStream` produces the `StoreEvents`. Various filters can be applied
/// to it to reduce which and how many events are delivered by the stream.
pub struct StoreEventStream<S> {
    source: S,
}

/// A boxed `StoreEventStream`
pub type StoreEventStreamBox =
    StoreEventStream<Box<dyn Stream<Item = Arc<StoreEvent>, Error = ()> + Send>>;

impl<S> Stream for StoreEventStream<S>
where
    S: Stream<Item = Arc<StoreEvent>, Error = ()> + Send,
{
    type Item = Arc<StoreEvent>;
    type Error = ();

    fn poll(&mut self) -> Result<Async<Option<Self::Item>>, Self::Error> {
        self.source.poll()
    }
}

impl<S> StoreEventStream<S>
where
    S: Stream<Item = Arc<StoreEvent>, Error = ()> + Send + 'static,
{
    // Create a new `StoreEventStream` from another such stream
    pub fn new(source: S) -> Self {
        StoreEventStream { source }
    }

    /// Filter a `StoreEventStream` by subgraph and entity. Only events that have
    /// at least one change to one of the given (subgraph, entity) combinations
    /// will be delivered by the filtered stream.
    pub fn filter_by_entities(self, filters: Vec<SubscriptionFilter>) -> StoreEventStreamBox {
        let source = self.source.filter(move |event| {
            event
                .changes
                .iter()
                .any(|change| filters.iter().any(|filter| filter.matches(change)))
        });

        StoreEventStream::new(Box::new(source))
    }

    /// Reduce the frequency with which events are generated while a
    /// subgraph deployment is syncing. While the given `deployment` is not
    /// synced yet, events from `source` are reported at most every
    /// `interval`. At the same time, no event is held for longer than
    /// `interval`. The `StoreEvents` that arrive during an interval appear
    /// on the returned stream as a single `StoreEvent`; the events are
    /// combined by using the maximum of all sources and the concatenation
    /// of the changes of the `StoreEvents` received during the interval.
    pub fn throttle_while_syncing(
        self,
        logger: &Logger,
        store: Arc<dyn QueryStore>,
        deployment: SubgraphDeploymentId,
        interval: Duration,
    ) -> StoreEventStreamBox {
        // We refresh the synced flag every SYNC_REFRESH_FREQ*interval to
        // avoid hitting the database too often to see if the subgraph has
        // been synced in the meantime. The only downside of this approach is
        // that we might continue throttling subscription updates for a little
        // bit longer than we really should
        static SYNC_REFRESH_FREQ: u32 = 4;

        // Check whether a deployment is marked as synced in the store
        let check_synced = |store: &dyn QueryStore, deployment: &SubgraphDeploymentId| {
            store.is_deployment_synced(deployment).unwrap_or(false)
        };
        let mut synced = check_synced(&*store, &deployment);
        let synced_check_interval = interval.checked_mul(SYNC_REFRESH_FREQ).unwrap();
        let mut synced_last_refreshed = Instant::now();

        let mut pending_event: Option<StoreEvent> = None;
        let mut source = self.source.fuse();
        let mut had_err = false;
        let mut delay = tokio::time::delay_for(interval).unit_error().compat();
        let logger = logger.clone();

        let source = Box::new(poll_fn(move || -> Poll<Option<Arc<StoreEvent>>, ()> {
            if had_err {
                // We had an error the last time through, but returned the pending
                // event first. Indicate the error now
                had_err = false;
                return Err(());
            }

            if !synced && synced_last_refreshed.elapsed() > synced_check_interval {
                synced = check_synced(&*store, &deployment);
                synced_last_refreshed = Instant::now();
            }

            if synced {
                return source.poll();
            }

            // Check if interval has passed since the last time we sent something.
            // If it has, start a new delay timer
            let should_send = match futures::future::Future::poll(&mut delay) {
                Ok(Async::NotReady) => false,
                // Timer errors are harmless. Treat them as if the timer had
                // become ready.
                Ok(Async::Ready(())) | Err(_) => {
                    delay = tokio::time::delay_for(interval).unit_error().compat();
                    true
                }
            };

            // Get as many events as we can off of the source stream
            loop {
                match source.poll() {
                    Ok(Async::NotReady) => {
                        if should_send && pending_event.is_some() {
                            let event = pending_event.take().map(|event| Arc::new(event));
                            return Ok(Async::Ready(event));
                        } else {
                            return Ok(Async::NotReady);
                        }
                    }
                    Ok(Async::Ready(None)) => {
                        let event = pending_event.take().map(|event| Arc::new(event));
                        return Ok(Async::Ready(event));
                    }
                    Ok(Async::Ready(Some(event))) => {
                        StoreEvent::accumulate(&logger, &mut pending_event, (*event).clone());
                    }
                    Err(()) => {
                        // Before we report the error, deliver what we have accumulated so far.
                        // We will report the error the next time poll() is called
                        if pending_event.is_some() {
                            had_err = true;
                            let event = pending_event.take().map(|event| Arc::new(event));
                            return Ok(Async::Ready(event));
                        } else {
                            return Err(());
                        }
                    }
                };
            }
        }));
        StoreEventStream::new(source)
    }
}

/// An entity operation that can be transacted into the store.
#[derive(Clone, Debug, PartialEq)]
pub enum EntityOperation {
    /// Locates the entity specified by `key` and sets its attributes according to the contents of
    /// `data`.  If no entity exists with this key, creates a new entity.
    Set { key: EntityKey, data: Entity },

    /// Removes an entity with the specified key, if one exists.
    Remove { key: EntityKey },
}

/// This is almost the same as `EntityKey`, but we only allow `MetadataType`
/// as the `entity_type`
#[derive(Clone, Debug)]
pub struct MetadataKey {
    /// ID of the subgraph.
    pub subgraph_id: SubgraphDeploymentId,

    /// Name of the entity type.
    pub entity_type: MetadataType,

    /// ID of the individual entity.
    pub entity_id: String,
}

impl From<MetadataKey> for EntityKey {
    fn from(key: MetadataKey) -> Self {
        EntityKey {
            subgraph_id: key.subgraph_id,
            entity_type: EntityType::Metadata(key.entity_type),
            entity_id: key.entity_id,
        }
    }
}

/// An operation on subgraph metadata. All operations implicitly only concern
/// the subgraph of subgraphs.
#[derive(Clone, Debug)]
pub enum MetadataOperation {
    /// Locates the entity with type `entity` and the given `id` in the
    /// subgraph of subgraphs and sets its attributes according to the
    /// contents of `data`.  If no such entity exists, creates a new entity.
    Set { key: MetadataKey, data: Entity },

    /// Removes an entity with the specified entity type and id if one exists.
    Remove { key: MetadataKey },
}

impl MetadataOperation {
    pub fn key(&self) -> &MetadataKey {
        use MetadataOperation::*;

        match self {
            Set { key, .. } | Remove { key, .. } => key,
        }
    }
}

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("store error: {0}")]
    Unknown(Error),
    #[error(
        "tried to set entity of type `{0}` with ID \"{1}\" but an entity of type `{2}`, \
         which has an interface in common with `{0}`, exists with the same ID"
    )]
    ConflictingId(String, String, String), // (entity, id, conflicting_entity)
    #[error("unknown field '{0}'")]
    UnknownField(String),
    #[error("unknown table '{0}'")]
    UnknownTable(String),
    #[error("malformed directive '{0}'")]
    MalformedDirective(String),
    #[error("query execution failed: {0}")]
    QueryExecutionError(String),
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),
    #[error(
        "subgraph `{0}` has already processed block `{1}`; \
         there are most likely two (or more) nodes indexing this subgraph"
    )]
    DuplicateBlockProcessing(SubgraphDeploymentId, u64),
    /// An internal error where we expected the application logic to enforce
    /// some constraint, e.g., that subgraph names are unique, but found that
    /// constraint to not hold
    #[error("internal constraint violated: {0}")]
    ConstraintViolation(String),
    #[error("deployment not found: {0}")]
    DeploymentNotFound(String),
    #[error("shard not found: {0} (this usually indicates a misconfiguration)")]
    UnknownShard(String),
    #[error("Fulltext search not yet deterministic")]
    FulltextSearchNonDeterministic,
}

// Convenience to report a constraint violation
#[macro_export]
macro_rules! constraint_violation {
    ($msg:expr) => {{
        StoreError::ConstraintViolation(format!("{}", $msg))
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        StoreError::ConstraintViolation(format!($fmt, $($arg)*))
    }}
}

impl From<::diesel::result::Error> for StoreError {
    fn from(e: ::diesel::result::Error) -> Self {
        StoreError::Unknown(e.into())
    }
}

impl From<::diesel::r2d2::PoolError> for StoreError {
    fn from(e: ::diesel::r2d2::PoolError) -> Self {
        StoreError::Unknown(e.into())
    }
}

impl From<Error> for StoreError {
    fn from(e: Error) -> Self {
        StoreError::Unknown(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Unknown(e.into())
    }
}

impl From<QueryExecutionError> for StoreError {
    fn from(e: QueryExecutionError) -> Self {
        StoreError::QueryExecutionError(e.to_string())
    }
}

pub struct StoredDynamicDataSource {
    pub name: String,
    pub source: Source,
    pub context: Option<String>,
    pub creation_block: Option<u64>,
}

pub trait SubscriptionManager: Send + Sync + 'static {
    /// Subscribe to changes for specific subgraphs and entities.
    ///
    /// Returns a stream of store events that match the input arguments.
    fn subscribe(&self, entities: Vec<SubscriptionFilter>) -> StoreEventStreamBox;
}

/// Common trait for store implementations.
#[async_trait]
pub trait SubgraphStore: Send + Sync + 'static {
    /// Get a pointer to the most recently processed block in the subgraph.
    fn block_ptr(
        &self,
        subgraph_id: &SubgraphDeploymentId,
    ) -> Result<Option<EthereumBlockPointer>, Error>;

    fn supports_proof_of_indexing<'a>(
        self: Arc<Self>,
        subgraph_id: &'a SubgraphDeploymentId,
    ) -> DynTryFuture<'a, bool>;

    /// A value of None indicates that the table is not available. Re-deploying
    /// the subgraph fixes this. It is undesirable to force everything to
    /// re-sync from scratch, so existing deployments will continue without a
    /// Proof of Indexing. Once all subgraphs have been re-deployed the Option
    /// can be removed.
    fn get_proof_of_indexing<'a>(
        self: Arc<Self>,
        subgraph_id: &'a SubgraphDeploymentId,
        indexer: &'a Option<Address>,
        block: EthereumBlockPointer,
    ) -> DynTryFuture<'a, Option<[u8; 32]>>;

    /// Looks up an entity using the given store key at the latest block.
    fn get(&self, key: EntityKey) -> Result<Option<Entity>, QueryExecutionError>;

    /// Look up multiple entities as of the latest block. Returns a map of
    /// entities by type.
    fn get_many(
        &self,
        subgraph_id: &SubgraphDeploymentId,
        ids_for_type: BTreeMap<&EntityType, Vec<&str>>,
    ) -> Result<BTreeMap<EntityType, Vec<Entity>>, StoreError>;

    /// Queries the store for entities that match the store query.
    fn find(&self, query: EntityQuery) -> Result<Vec<Entity>, QueryExecutionError>;

    /// Queries the store for a single entity matching the store query.
    fn find_one(&self, query: EntityQuery) -> Result<Option<Entity>, QueryExecutionError>;

    /// Find the reverse of keccak256 for `hash` through looking it up in the
    /// rainbow table.
    fn find_ens_name(&self, _hash: &str) -> Result<Option<String>, QueryExecutionError>;

    /// Transact the entity changes from a single block atomically into the store, and update the
    /// subgraph block pointer to `block_ptr_to`.
    ///
    /// `block_ptr_to` must point to a child block of the current subgraph block pointer.
    fn transact_block_operations(
        &self,
        subgraph_id: SubgraphDeploymentId,
        block_ptr_to: EthereumBlockPointer,
        mods: Vec<EntityModification>,
        stopwatch: StopwatchMetrics,
        deterministic_errors: Vec<SubgraphError>,
    ) -> Result<(), StoreError>;

    /// Revert the entity changes from a single block atomically in the store, and update the
    /// subgraph block pointer to `block_ptr_to`.
    ///
    /// `block_ptr_to` must point to the parent block of the subgraph block pointer.
    fn revert_block_operations(
        &self,
        subgraph_id: SubgraphDeploymentId,
        block_ptr_to: EthereumBlockPointer,
    ) -> Result<(), StoreError>;

    /// Find the deployment for the current version of subgraph `name` and
    /// return details about it needed for executing queries
    fn deployment_state_from_name(&self, name: SubgraphName)
        -> Result<DeploymentState, StoreError>;

    /// Find the deployment for the subgraph deployment `id` and
    /// return details about it needed for executing queries
    fn deployment_state_from_id(
        &self,
        id: SubgraphDeploymentId,
    ) -> Result<DeploymentState, StoreError>;

    /// Set subgraph status to failed with the given error as the cause.
    async fn fail_subgraph(
        &self,
        id: SubgraphDeploymentId,
        error: SubgraphError,
    ) -> Result<(), StoreError>;

    /// Check if the store is accepting queries for the specified subgraph.
    /// May return true even if the specified subgraph is not currently assigned to an indexing
    /// node, as the store will still accept queries.
    fn is_deployed(&self, id: &SubgraphDeploymentId) -> Result<bool, Error> {
        self.block_ptr(id).map(|ptr| ptr.is_some())
    }

    /// Return true if the deployment with the given id is fully synced,
    /// and return false otherwise. Errors from the store are passed back up
    fn is_deployment_synced(&self, id: &SubgraphDeploymentId) -> Result<bool, Error>;

    /// The deployment `id` finished syncing, mark it as synced in the database
    /// and promote it to the current version in the subgraphs where it was the
    /// pending version so far
    fn deployment_synced(&self, id: &SubgraphDeploymentId) -> Result<(), Error>;

    /// Create a new deployment for the subgraph `name`. If the deployment
    /// already exists (as identified by the `schema.id`), reuse that, otherwise
    /// create a new deployment, and point the current or pending version of
    /// `name` at it, depending on the `mode`
    fn create_subgraph_deployment(
        &self,
        name: SubgraphName,
        schema: &Schema,
        deployment: SubgraphDeploymentEntity,
        node_id: NodeId,
        network: String,
        mode: SubgraphVersionSwitchingMode,
    ) -> Result<(), StoreError>;

    /// Create a new subgraph with the given name. If one already exists, use
    /// the existing one. Return the `id` of the newly created or existing
    /// subgraph
    fn create_subgraph(&self, name: SubgraphName) -> Result<String, StoreError>;

    /// Remove a subgraph and all its versions; if deployments that were used
    /// by this subgraph do not need to be indexed anymore, also remove
    /// their assignment, but keep the deployments themselves around
    fn remove_subgraph(&self, name: SubgraphName) -> Result<(), StoreError>;

    /// Assign the subgraph with `id` to the node `node_id`. If there is no
    /// assignment for the given deployment, report an error.
    fn reassign_subgraph(
        &self,
        id: &SubgraphDeploymentId,
        node_id: &NodeId,
    ) -> Result<(), StoreError>;

    fn unassign_subgraph(&self, id: &SubgraphDeploymentId) -> Result<(), StoreError>;

    /// Start an existing subgraph deployment. This will reset the state of
    /// the subgraph to a known good state. `ops` needs to contain all the
    /// operations on the subgraph of subgraphs to reset the metadata of the
    /// subgraph
    fn start_subgraph_deployment(
        &self,
        logger: &Logger,
        subgraph_id: &SubgraphDeploymentId,
    ) -> Result<(), StoreError>;

    /// Load the dynamic data sources for the given deployment
    async fn load_dynamic_data_sources(
        &self,
        subgraph_id: SubgraphDeploymentId,
    ) -> Result<Vec<StoredDynamicDataSource>, StoreError>;

    fn assigned_node(
        &self,
        subgraph_id: &SubgraphDeploymentId,
    ) -> Result<Option<NodeId>, StoreError>;

    fn assignments(&self, node: &NodeId) -> Result<Vec<SubgraphDeploymentId>, StoreError>;

    /// Return `true` if a subgraph `name` exists, regardless of whether the
    /// subgraph has any deployments attached to it
    fn subgraph_exists(&self, name: &SubgraphName) -> Result<bool, StoreError>;

    /// Return the GraphQL schema supplied by the user
    fn input_schema(&self, subgraph_id: &SubgraphDeploymentId) -> Result<Arc<Schema>, StoreError>;

    /// Return the GraphQL schema that was derived from the user's schema by
    /// adding a root query type etc. to it
    fn api_schema(&self, subgraph_id: &SubgraphDeploymentId) -> Result<Arc<ApiSchema>, StoreError>;

    /// Return the name of the network that the subgraph is indexing from. The
    /// names returned are things like `mainnet` or `ropsten`
    fn network_name(&self, subgraph_id: &SubgraphDeploymentId) -> Result<String, StoreError>;
}

pub trait QueryStoreManager: Send + Sync + 'static {
    /// Get a new `QueryStore`. A `QueryStore` is tied to a DB replica, so if Graph Node is
    /// configured to use secondary DB servers the queries will be distributed between servers.
    ///
    /// The query store is specific to a deployment, and `id` must indicate
    /// which deployment will be queried. It is not possible to use the id of the
    /// metadata subgraph, though the resulting store can be used to query
    /// metadata about the deployment `id` (but not metadata about other deployments).
    ///
    /// If `for_subscription` is true, the main replica will always be used.
    fn query_store(
        &self,
        target: QueryTarget,
        for_subscription: bool,
    ) -> Result<Arc<dyn QueryStore + Send + Sync>, QueryExecutionError>;
}

mock! {
    pub Store {
        fn get_many_mock<'a>(
            &self,
            _subgraph_id: &SubgraphDeploymentId,
            _ids_for_type: BTreeMap<&'a EntityType, Vec<&'a str>>,
        ) -> Result<BTreeMap<EntityType, Vec<Entity>>, StoreError>;
    }
}

// The type that the connection pool uses to track wait times for
// connection checkouts
pub type PoolWaitStats = Arc<RwLock<MovingStats>>;

// The store trait must be implemented manually because mockall does not support async_trait, nor borrowing from arguments.
#[async_trait]
impl SubgraphStore for MockStore {
    fn block_ptr(
        &self,
        _subgraph_id: &SubgraphDeploymentId,
    ) -> Result<Option<EthereumBlockPointer>, Error> {
        unimplemented!();
    }

    fn supports_proof_of_indexing<'a>(
        self: Arc<Self>,
        _subgraph_id: &'a SubgraphDeploymentId,
    ) -> DynTryFuture<'a, bool> {
        unimplemented!();
    }

    fn get_proof_of_indexing<'a>(
        self: Arc<Self>,
        _subgraph_id: &'a SubgraphDeploymentId,
        _indexer: &'a Option<Address>,
        _block: EthereumBlockPointer,
    ) -> DynTryFuture<'a, Option<[u8; 32]>> {
        unimplemented!();
    }

    fn get(&self, _key: EntityKey) -> Result<Option<Entity>, QueryExecutionError> {
        unimplemented!()
    }

    fn get_many(
        &self,
        subgraph_id: &SubgraphDeploymentId,
        ids_for_type: BTreeMap<&EntityType, Vec<&str>>,
    ) -> Result<BTreeMap<EntityType, Vec<Entity>>, StoreError> {
        self.get_many_mock(subgraph_id, ids_for_type)
    }

    fn find(&self, _query: EntityQuery) -> Result<Vec<Entity>, QueryExecutionError> {
        unimplemented!()
    }

    fn find_one(&self, _query: EntityQuery) -> Result<Option<Entity>, QueryExecutionError> {
        unimplemented!()
    }

    fn find_ens_name(&self, _hash: &str) -> Result<Option<String>, QueryExecutionError> {
        unimplemented!()
    }

    fn transact_block_operations(
        &self,
        _subgraph_id: SubgraphDeploymentId,
        _block_ptr_to: EthereumBlockPointer,
        _mods: Vec<EntityModification>,
        _stopwatch: StopwatchMetrics,
        _deterministic_errors: Vec<SubgraphError>,
    ) -> Result<(), StoreError> {
        unimplemented!()
    }

    fn revert_block_operations(
        &self,
        _subgraph_id: SubgraphDeploymentId,
        _block_ptr_to: EthereumBlockPointer,
    ) -> Result<(), StoreError> {
        unimplemented!()
    }

    fn deployment_state_from_name(&self, _: SubgraphName) -> Result<DeploymentState, StoreError> {
        unimplemented!()
    }

    fn deployment_state_from_id(
        &self,
        _: SubgraphDeploymentId,
    ) -> Result<DeploymentState, StoreError> {
        unimplemented!()
    }

    async fn fail_subgraph(
        &self,
        _: SubgraphDeploymentId,
        _: SubgraphError,
    ) -> Result<(), StoreError> {
        unimplemented!()
    }

    fn create_subgraph_deployment(
        &self,
        _: SubgraphName,
        _: &Schema,
        _: SubgraphDeploymentEntity,
        _: NodeId,
        _: String,
        _: SubgraphVersionSwitchingMode,
    ) -> Result<(), StoreError> {
        unimplemented!()
    }

    fn create_subgraph(&self, _: SubgraphName) -> Result<String, StoreError> {
        unimplemented!()
    }

    fn remove_subgraph(&self, _: SubgraphName) -> Result<(), StoreError> {
        unimplemented!()
    }

    fn reassign_subgraph(&self, _: &SubgraphDeploymentId, _: &NodeId) -> Result<(), StoreError> {
        unimplemented!()
    }

    fn unassign_subgraph(&self, _: &SubgraphDeploymentId) -> Result<(), StoreError> {
        unimplemented!()
    }

    fn start_subgraph_deployment(
        &self,
        _logger: &Logger,
        _subgraph_id: &SubgraphDeploymentId,
    ) -> Result<(), StoreError> {
        unimplemented!()
    }

    fn is_deployment_synced(&self, _: &SubgraphDeploymentId) -> Result<bool, Error> {
        unimplemented!()
    }

    fn deployment_synced(&self, _: &SubgraphDeploymentId) -> Result<(), Error> {
        unimplemented!()
    }

    async fn load_dynamic_data_sources(
        &self,
        _subgraph_id: SubgraphDeploymentId,
    ) -> Result<Vec<StoredDynamicDataSource>, StoreError> {
        unimplemented!()
    }

    fn assigned_node(&self, _: &SubgraphDeploymentId) -> Result<Option<NodeId>, StoreError> {
        unimplemented!()
    }

    fn assignments(&self, _: &NodeId) -> Result<Vec<SubgraphDeploymentId>, StoreError> {
        unimplemented!()
    }

    fn subgraph_exists(&self, _: &SubgraphName) -> Result<bool, StoreError> {
        unimplemented!()
    }

    fn input_schema(&self, _: &SubgraphDeploymentId) -> Result<Arc<Schema>, StoreError> {
        unimplemented!()
    }

    fn api_schema(&self, _: &SubgraphDeploymentId) -> Result<Arc<ApiSchema>, StoreError> {
        unimplemented!()
    }

    fn network_name(&self, _: &SubgraphDeploymentId) -> Result<String, StoreError> {
        unimplemented!()
    }
}

pub trait BlockStore: Send + Sync + 'static {
    type ChainStore: ChainStore;

    fn chain_store(&self, network: &str) -> Option<Arc<Self::ChainStore>>;
}

pub trait CallCache: Send + Sync + 'static {
    type EthereumCallCache: EthereumCallCache;

    fn ethereum_call_cache(&self, network: &str) -> Option<Arc<Self::EthereumCallCache>>;
}

/// Common trait for blockchain store implementations.
#[automock]
pub trait ChainStore: Send + Sync + 'static {
    /// Get a pointer to this blockchain's genesis block.
    fn genesis_block_ptr(&self) -> Result<EthereumBlockPointer, Error>;

    /// Insert blocks into the store (or update if they are already present).
    fn upsert_blocks<B, E>(
        &self,
        _blocks: B,
    ) -> Box<dyn Future<Item = (), Error = E> + Send + 'static>
    where
        B: Stream<Item = EthereumBlock, Error = E> + Send + 'static,
        E: From<Error> + Send + 'static,
        Self: Sized,
    {
        unimplemented!()
    }

    fn upsert_light_blocks(&self, blocks: Vec<LightEthereumBlock>) -> Result<(), Error>;

    /// Try to update the head block pointer to the block with the highest block number.
    ///
    /// Only updates pointer if there is a block with a higher block number than the current head
    /// block, and the `ancestor_count` most recent ancestors of that block are in the store.
    /// Note that this means if the Ethereum node returns a different "latest block" with a
    /// different hash but same number, we do not update the chain head pointer.
    /// This situation can happen on e.g. Infura where requests are load balanced across many
    /// Ethereum nodes, in which case it's better not to continuously revert and reapply the latest
    /// blocks.
    ///
    /// If the pointer was updated, returns `Ok(vec![])`, and fires a HeadUpdateEvent.
    ///
    /// If no block has a number higher than the current head block, returns `Ok(vec![])`.
    ///
    /// If the candidate new head block had one or more missing ancestors, returns
    /// `Ok(missing_blocks)`, where `missing_blocks` is a nonexhaustive list of missing blocks.
    fn attempt_chain_head_update(&self, ancestor_count: u64) -> Result<Vec<H256>, Error>;

    /// Subscribe to chain head updates.
    fn chain_head_updates(&self) -> ChainHeadUpdateStream;

    /// Get the current head block pointer for this chain.
    /// Any changes to the head block pointer will be to a block with a larger block number, never
    /// to a block with a smaller or equal block number.
    ///
    /// The head block pointer will be None on initial set up.
    fn chain_head_ptr(&self) -> Result<Option<EthereumBlockPointer>, Error>;

    /// Returns the blocks present in the store.
    fn blocks(&self, hashes: Vec<H256>) -> Result<Vec<LightEthereumBlock>, Error>;

    /// Get the `offset`th ancestor of `block_hash`, where offset=0 means the block matching
    /// `block_hash` and offset=1 means its parent. Returns None if unable to complete due to
    /// missing blocks in the chain store.
    ///
    /// Returns an error if the offset would reach past the genesis block.
    fn ancestor_block(
        &self,
        block_ptr: EthereumBlockPointer,
        offset: u64,
    ) -> Result<Option<EthereumBlock>, Error>;

    /// Remove old blocks from the cache we maintain in the database and
    /// return a pair containing the number of the oldest block retained
    /// and the number of blocks deleted.
    /// We will never remove blocks that are within `ancestor_count` of
    /// the chain head.
    fn cleanup_cached_blocks(&self, ancestor_count: u64) -> Result<(BlockNumber, usize), Error>;

    /// Return the hashes of all blocks with the given number
    fn block_hashes_by_block_number(&self, number: u64) -> Result<Vec<H256>, Error>;

    /// Confirm that block number `number` has hash `hash` and that the store
    /// may purge any other blocks with that number
    fn confirm_block_hash(&self, number: u64, hash: &H256) -> Result<usize, Error>;

    /// Find the block with `block_hash` and return the network name and number
    fn block_number(&self, block_hash: H256) -> Result<Option<(String, BlockNumber)>, StoreError>;
}

pub trait EthereumCallCache: Send + Sync + 'static {
    /// Cached return value.
    fn get_call(
        &self,
        contract_address: ethabi::Address,
        encoded_call: &[u8],
        block: EthereumBlockPointer,
    ) -> Result<Option<Vec<u8>>, Error>;

    // Add entry to the cache.
    fn set_call(
        &self,
        contract_address: ethabi::Address,
        encoded_call: &[u8],
        block: EthereumBlockPointer,
        return_value: &[u8],
    ) -> Result<(), Error>;
}

/// Store operations used when serving queries for a specific deployment
#[async_trait]
pub trait QueryStore: Send + Sync {
    fn find_query_values(
        &self,
        query: EntityQuery,
    ) -> Result<Vec<BTreeMap<String, q::Value>>, QueryExecutionError>;

    fn is_deployment_synced(&self, id: &SubgraphDeploymentId) -> Result<bool, Error>;

    fn block_ptr(
        &self,
        subgraph_id: SubgraphDeploymentId,
    ) -> Result<Option<EthereumBlockPointer>, Error>;

    fn block_number(&self, block_hash: H256) -> Result<Option<BlockNumber>, StoreError>;

    fn wait_stats(&self) -> &PoolWaitStats;

    /// If `block` is `None`, assumes the latest block.
    async fn has_non_fatal_errors(
        &self,
        id: SubgraphDeploymentId,
        block: Option<BlockNumber>,
    ) -> Result<bool, StoreError>;

    /// Find the current state for the subgraph deployment `id` and
    /// return details about it needed for executing queries
    fn deployment_state(&self) -> Result<DeploymentState, QueryExecutionError>;

    fn api_schema(&self) -> Result<Arc<ApiSchema>, QueryExecutionError>;

    fn network_name(&self) -> &str;
}

/// A view of the store that can provide information about the indexing status
/// of any subgraph and any deployment
pub trait StatusStore: Send + Sync + 'static {
    fn status(&self, filter: status::Filter) -> Result<Vec<status::Info>, StoreError>;

    /// Support for the explorer-specific API
    fn version_info(&self, version_id: &str) -> Result<VersionInfo, StoreError>;

    /// Support for the explorer-specific API; note that `subgraph_id` must be
    /// the id of an entry in `subgraphs.subgraph`, not that of a deployment.
    /// The return values are the ids of the `subgraphs.subgraph_version` for
    /// the current and pending versions of the subgraph
    fn versions_for_subgraph_id(
        &self,
        subgraph_id: &str,
    ) -> Result<(Option<String>, Option<String>), StoreError>;

    fn supports_proof_of_indexing<'a>(
        self: Arc<Self>,
        subgraph_id: &'a SubgraphDeploymentId,
    ) -> DynTryFuture<'a, bool>;

    /// A value of None indicates that the table is not available. Re-deploying
    /// the subgraph fixes this. It is undesirable to force everything to
    /// re-sync from scratch, so existing deployments will continue without a
    /// Proof of Indexing. Once all subgraphs have been re-deployed the Option
    /// can be removed.
    fn get_proof_of_indexing<'a>(
        self: Arc<Self>,
        subgraph_id: &'a SubgraphDeploymentId,
        indexer: &'a Option<Address>,
        block: EthereumBlockPointer,
    ) -> DynTryFuture<'a, Option<[u8; 32]>>;
}

/// An entity operation that can be transacted into the store; as opposed to
/// `EntityOperation`, we already know whether a `Set` should be an `Insert`
/// or `Update`
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityModification {
    /// Insert the entity
    Insert { key: EntityKey, data: Entity },
    /// Update the entity by overwriting it
    Overwrite { key: EntityKey, data: Entity },
    /// Remove the entity
    Remove { key: EntityKey },
}

impl EntityModification {
    pub fn entity_key(&self) -> &EntityKey {
        use EntityModification::*;
        match self {
            Insert { key, .. } | Overwrite { key, .. } | Remove { key } => key,
        }
    }

    pub fn is_remove(&self) -> bool {
        match self {
            EntityModification::Remove { .. } => true,
            _ => false,
        }
    }
}

/// A representation of entity operations that can be accumulated.
#[derive(Debug, Clone)]
enum EntityOp {
    Remove,
    Update(Entity),
    Overwrite(Entity),
}

impl EntityOp {
    fn apply_to(self, entity: Option<Entity>) -> Option<Entity> {
        use EntityOp::*;
        match (self, entity) {
            (Remove, _) => None,
            (Overwrite(new), _) | (Update(new), None) => Some(new),
            (Update(updates), Some(mut entity)) => {
                entity.merge_remove_null_fields(updates);
                Some(entity)
            }
        }
    }

    fn accumulate(&mut self, next: EntityOp) {
        use EntityOp::*;
        let update = match next {
            // Remove and Overwrite ignore the current value.
            Remove | Overwrite(_) => {
                *self = next;
                return;
            }
            Update(update) => update,
        };

        // We have an update, apply it.
        match self {
            // This is how `Overwrite` is constructed, by accumulating `Update` onto `Remove`.
            Remove => *self = Overwrite(update),
            Update(current) | Overwrite(current) => current.merge(update),
        }
    }
}

/// A cache for entities from the store that provides the basic functionality
/// needed for the store interactions in the host exports. This struct tracks
/// how entities are modified, and caches all entities looked up from the
/// store. The cache makes sure that
///   (1) no entity appears in more than one operation
///   (2) only entities that will actually be changed from what they
///       are in the store are changed
pub struct EntityCache {
    /// The state of entities in the store. An entry of `None`
    /// means that the entity is not present in the store
    current: LfuCache<EntityKey, Option<Entity>>,

    /// The accumulated changes to an entity.
    updates: HashMap<EntityKey, EntityOp>,

    // Updates for a currently executing handler.
    handler_updates: HashMap<EntityKey, EntityOp>,

    // Marks whether updates should go in `handler_updates`.
    in_handler: bool,

    /// The store is only used to read entities.
    pub store: Arc<dyn SubgraphStore>,
}

impl Debug for EntityCache {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("EntityCache")
            .field("current", &self.current)
            .field("updates", &self.updates)
            .finish()
    }
}

pub struct ModificationsAndCache {
    pub modifications: Vec<EntityModification>,
    pub entity_lfu_cache: LfuCache<EntityKey, Option<Entity>>,
}

impl EntityCache {
    pub fn new(store: Arc<dyn SubgraphStore>) -> Self {
        Self {
            current: LfuCache::new(),
            updates: HashMap::new(),
            handler_updates: HashMap::new(),
            in_handler: false,
            store,
        }
    }

    pub fn with_current(
        store: Arc<dyn SubgraphStore>,
        current: LfuCache<EntityKey, Option<Entity>>,
    ) -> EntityCache {
        EntityCache {
            current,
            updates: HashMap::new(),
            handler_updates: HashMap::new(),
            in_handler: false,
            store,
        }
    }

    pub(crate) fn enter_handler(&mut self) {
        assert!(!self.in_handler);
        self.in_handler = true;
    }

    pub(crate) fn exit_handler(&mut self) {
        assert!(self.in_handler);
        self.in_handler = false;

        // Apply all handler updates to the main `updates`.
        let handler_updates = Vec::from_iter(self.handler_updates.drain());
        for (key, op) in handler_updates {
            self.entity_op(key, op)
        }
    }

    pub(crate) fn exit_handler_and_discard_changes(&mut self) {
        assert!(self.in_handler);
        self.in_handler = false;
        self.handler_updates.clear();
    }

    pub fn get(&mut self, key: &EntityKey) -> Result<Option<Entity>, QueryExecutionError> {
        // Get the current entity, apply any updates from `updates`, then from `handler_updates`.
        let mut entity = self.current.get_entity(&*self.store, &key)?;
        if let Some(op) = self.updates.get(&key).cloned() {
            entity = op.apply_to(entity)
        }
        if let Some(op) = self.handler_updates.get(&key).cloned() {
            entity = op.apply_to(entity)
        }
        Ok(entity)
    }

    pub fn remove(&mut self, key: EntityKey) {
        self.entity_op(key, EntityOp::Remove);
    }

    pub fn set(&mut self, key: EntityKey, entity: Entity) {
        self.entity_op(key, EntityOp::Update(entity))
    }

    pub fn append(&mut self, operations: Vec<EntityOperation>) {
        assert!(!self.in_handler);

        for operation in operations {
            match operation {
                EntityOperation::Set { key, data } => {
                    self.entity_op(key, EntityOp::Update(data));
                }
                EntityOperation::Remove { key } => {
                    self.entity_op(key, EntityOp::Remove);
                }
            }
        }
    }

    fn entity_op(&mut self, key: EntityKey, op: EntityOp) {
        use std::collections::hash_map::Entry;

        let updates = match self.in_handler {
            true => &mut self.handler_updates,
            false => &mut self.updates,
        };

        match updates.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(op);
            }
            Entry::Occupied(mut entry) => entry.get_mut().accumulate(op),
        }
    }

    pub(crate) fn extend(&mut self, other: EntityCache) {
        assert!(!other.in_handler);

        self.current.extend(other.current);
        for (key, op) in other.updates {
            self.entity_op(key, op);
        }
    }

    /// Return the changes that have been made via `set` and `remove` as
    /// `EntityModification`, making sure to only produce one when a change
    /// to the current state is actually needed.
    ///
    /// Also returns the updated `LfuCache`.
    pub fn as_modifications(
        mut self,
        store: &(impl SubgraphStore + ?Sized),
    ) -> Result<ModificationsAndCache, QueryExecutionError> {
        assert!(!self.in_handler);

        // The first step is to make sure all entities being set are in `self.current`.
        // For each subgraph, we need a map of entity type to missing entity ids.
        let missing = self
            .updates
            .keys()
            .filter(|key| !self.current.contains_key(key));

        let mut missing_by_subgraph: BTreeMap<_, BTreeMap<&EntityType, Vec<&str>>> =
            BTreeMap::new();
        for key in missing {
            missing_by_subgraph
                .entry(&key.subgraph_id)
                .or_default()
                .entry(&key.entity_type)
                .or_default()
                .push(&key.entity_id);
        }

        for (subgraph_id, keys) in missing_by_subgraph {
            for (entity_type, entities) in store.get_many(subgraph_id, keys)? {
                for entity in entities {
                    let key = EntityKey {
                        subgraph_id: subgraph_id.clone(),
                        entity_type: entity_type.clone(),
                        entity_id: entity.id().unwrap(),
                    };
                    self.current.insert(key, Some(entity));
                }
            }
        }

        let mut mods = Vec::new();
        for (key, update) in self.updates {
            use EntityModification::*;
            let current = self.current.remove(&key).and_then(|entity| entity);
            let modification = match (current, update) {
                // Entity was created
                (None, EntityOp::Update(updates)) | (None, EntityOp::Overwrite(updates)) => {
                    // Merging with an empty entity removes null fields.
                    let mut data = Entity::new();
                    data.merge_remove_null_fields(updates);
                    self.current.insert(key.clone(), Some(data.clone()));
                    Some(Insert { key, data })
                }
                // Entity may have been changed
                (Some(current), EntityOp::Update(updates)) => {
                    let mut data = current.clone();
                    data.merge_remove_null_fields(updates);
                    self.current.insert(key.clone(), Some(data.clone()));
                    if current != data {
                        Some(Overwrite { key, data })
                    } else {
                        None
                    }
                }
                // Entity was removed and then updated, so it will be overwritten
                (Some(current), EntityOp::Overwrite(data)) => {
                    self.current.insert(key.clone(), Some(data.clone()));
                    if current != data {
                        Some(Overwrite { key, data })
                    } else {
                        None
                    }
                }
                // Existing entity was deleted
                (Some(_), EntityOp::Remove) => {
                    self.current.insert(key.clone(), None);
                    Some(Remove { key })
                }
                // Entity was deleted, but it doesn't exist in the store
                (None, EntityOp::Remove) => None,
            };
            if let Some(modification) = modification {
                mods.push(modification)
            }
        }
        Ok(ModificationsAndCache {
            modifications: mods,
            entity_lfu_cache: self.current,
        })
    }
}

impl LfuCache<EntityKey, Option<Entity>> {
    // Helper for cached lookup of an entity.
    fn get_entity(
        &mut self,
        store: &(impl SubgraphStore + ?Sized),
        key: &EntityKey,
    ) -> Result<Option<Entity>, QueryExecutionError> {
        match self.get(&key) {
            None => {
                let mut entity = store.get(key.clone())?;
                if let Some(entity) = &mut entity {
                    // `__typename` is for queries not for mappings.
                    entity.remove("__typename");
                }
                self.insert(key.clone(), entity.clone());
                Ok(entity)
            }
            Some(data) => Ok(data.to_owned()),
        }
    }
}

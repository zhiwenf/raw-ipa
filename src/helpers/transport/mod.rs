use std::any::Any;
use std::borrow::Borrow;
use crate::{
    helpers::HelperIdentity,
    protocol::{QueryId, Step},
};
use async_trait::async_trait;
use futures::Stream;
use std::io;
use std::ops::Deref;
use std::sync::Weak;

mod bytearrstream;
pub mod query;

pub use bytearrstream::{AlignedByteArrStream, ByteArrStream};

pub trait ResourceIdentifier: Sized {}
pub trait QueryIdBinding: Sized
where
    Option<QueryId>: From<Self>,
{
}
pub trait StepBinding: Sized
where
    Option<Step>: From<Self>,
{
}

pub struct NoResourceIdentifier;
pub struct NoQueryId;
pub struct NoStep;

#[derive(Debug, Copy, Clone)]
pub enum RouteId {
    Records,
    ReceiveQuery,
    PrepareQuery,
}

impl ResourceIdentifier for NoResourceIdentifier {}
impl ResourceIdentifier for RouteId {}

impl From<NoQueryId> for Option<QueryId> {
    fn from(_: NoQueryId) -> Self {
        None
    }
}

impl QueryIdBinding for NoQueryId {}
impl QueryIdBinding for QueryId {}

impl From<NoStep> for Option<Step> {
    fn from(_: NoStep) -> Self {
        None
    }
}

impl StepBinding for NoStep {}
impl StepBinding for Step {}

pub trait RouteParams<R: ResourceIdentifier, Q: QueryIdBinding, S: StepBinding>: Send
where
    Option<QueryId>: From<Q>,
    Option<Step>: From<S>,
{
    type Params: Borrow<str>;

    fn resource_identifier(&self) -> R;
    fn query_id(&self) -> Q;
    fn step(&self) -> S;

    fn extra(&self) -> Self::Params;
}

impl RouteParams<NoResourceIdentifier, QueryId, Step> for (QueryId, Step) {
    type Params = &'static str;

    fn resource_identifier(&self) -> NoResourceIdentifier {
        NoResourceIdentifier
    }

    fn query_id(&self) -> QueryId {
        self.0
    }

    fn step(&self) -> Step {
        self.1.clone()
    }

    fn extra(&self) -> Self::Params {
        ""
    }
}

impl RouteParams<RouteId, QueryId, Step> for (RouteId, QueryId, Step) {
    type Params = &'static str;

    fn resource_identifier(&self) -> RouteId {
        self.0
    }

    fn query_id(&self) -> QueryId {
        self.1
    }

    fn step(&self) -> Step {
        self.2.clone()
    }

    fn extra(&self) -> Self::Params {
        ""
    }
}

/// Transport that supports per-query,per-step channels
#[async_trait]
pub trait Transport: Clone + Send + Sync + 'static {
    type RecordsStream: Stream<Item = Vec<u8>> + Send + Unpin;

    fn identity(&self) -> HelperIdentity;

    /// Sends a new request to the given destination helper party.
    /// The contract for this method requires it to block until the request is acknowledged by
    /// the remote party. For streaming requests where body is large, only request headers are
    /// expected to be acknowledged)
    async fn send<D, Q, S, R>(
        &self,
        dest: HelperIdentity,
        route: R,
        data: D,
    ) -> Result<(), io::Error>
    where
        Option<QueryId>: From<Q>,
        Option<Step>: From<S>,
        Q: QueryIdBinding,
        S: StepBinding,
        R: RouteParams<RouteId, Q, S>,
        D: Stream<Item = Vec<u8>> + Send + 'static;

    /// Return the stream of records to be received from another helper for the specific query
    /// and step
    fn receive<R: RouteParams<NoResourceIdentifier, QueryId, Step>>(
        &self,
        from: HelperIdentity,
        route: R,
    ) -> Self::RecordsStream;
}

/// Enum to dispatch calls to various [`Transport`] implementations without the need
/// of dynamic dispatch. DD is not even possible with this trait, so that is the only way to prevent
/// [`Gateway`] to be generic over it. We want to avoid that as it pollutes our protocol code.
///
/// [`Gateway`]: crate::helpers::Gateway
#[derive(Clone)]
pub enum TransportImpl {
    #[cfg(any(test, feature = "test-fixture"))]
    InMemory(std::sync::Weak<crate::test_fixture::network::InMemoryTransport>),
    #[cfg(not(any(test, feature = "test-fixture")))]
    RealWorld,
}

#[async_trait]
#[allow(unused_variables)]
impl Transport for TransportImpl {
    #[cfg(any(test, feature = "test-fixture"))]
    type RecordsStream = <std::sync::Weak<crate::test_fixture::network::InMemoryTransport> as Transport>::RecordsStream;
    #[cfg(not(any(test, feature = "test-fixture")))]
    type RecordsStream = std::pin::Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>;

    fn identity(&self) -> HelperIdentity {
        match self {
            #[cfg(any(test, feature = "test-fixture"))]
            TransportImpl::InMemory(ref inner) => inner.identity(),
            #[cfg(not(any(test, feature = "test-fixture")))]
            TransportImpl::RealWorld => {
                unimplemented!()
            }
        }
    }

    async fn send<D, Q, S, R>(
        &self,
        dest: HelperIdentity,
        route: R,
        data: D,
    ) -> Result<(), std::io::Error>
    where
        Option<QueryId>: From<Q>,
        Option<Step>: From<S>,
        Q: QueryIdBinding,
        S: StepBinding,
        R: RouteParams<RouteId, Q, S>,
        D: Stream<Item = Vec<u8>> + Send + 'static,
    {
        match self {
            #[cfg(any(test, feature = "test-fixture"))]
            TransportImpl::InMemory(inner) => inner.send(dest, route, data).await,
            #[cfg(not(any(test, feature = "test-fixture")))]
            TransportImpl::RealWorld => {
                unimplemented!()
            }
        }
    }

    fn receive<R: RouteParams<NoResourceIdentifier, QueryId, Step>>(
        &self,
        from: HelperIdentity,
        route: R,
    ) -> Self::RecordsStream {
        match self {
            #[cfg(any(test, feature = "test-fixture"))]
            TransportImpl::InMemory(inner) => inner.receive(from, route),
            #[cfg(not(any(test, feature = "test-fixture")))]
            TransportImpl::RealWorld => {
                unimplemented!()
            }
        }
    }
}

// impl <T: Transport + Any> From<&T> for TransportImpl {
//     fn from(value: &T) -> Self {
//         TransportImpl::from(value)
//     }
// }

// impl TransportImpl {
//     #[cfg(any(feature = "test-fixture", test))]
//     pub fn from<T: Transport + Any>(value: &T) -> Self {
//         use crate::test_fixture::network::InMemoryTransport;
//         let value_any = value as &dyn Any;
//         match value_any.downcast_ref::<Deref<InMemoryTransport>>() {
//             Some(transport) => {Self::InMemory(transport.clone())}
//             None => panic!("Only InMemory transport is supported inside the gateway at the moment")
//         }
//     }
// }

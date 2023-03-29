use crate::{
    helpers::{
        query::{PrepareQuery, QueryCommand, QueryConfig, QueryInput},
        Gateway, GatewayConfig, HelperIdentity, Role, RoleAssignment,
    },
    protocol::QueryId,
    query::{
        executor,
        state::{QueryState, QueryStatus, RunningQueries, StateError},
        ProtocolResult,
    },
};
use futures::StreamExt;
use futures_util::future::try_join;
use pin_project::pin_project;
use std::{collections::hash_map::Entry, fmt::{Debug, Formatter}, io};
use futures_util::stream;
use tokio::sync::oneshot;
use crate::helpers::{RouteId, Transport, TransportImpl};

/// `Processor` accepts and tracks requests to initiate new queries on this helper party
/// network. It makes sure queries are coordinated and each party starts processing it when
/// it has all the information required.
///
/// Query processing consists of multiple steps:
/// - A new request to initiate a query arrives from an external party (report collector) to any of the
/// helpers.
/// - Upon receiving that request, helper chooses a unique [`QueryId`] and assigns [`Role`] to every
/// helper. It informs other parties about it and awaits their response.
/// - If all parties accept the proposed query, they negotiate shared randomness and signal that
/// - they're ready to receive inputs.
/// - Each party, upon receiving the input as a set of [`AdditiveShare`], immediately starts executing
/// IPA protocol.
/// - When helper party is done, it holds onto the results of the computation until the external party
/// that initiated this request asks for them.
///
/// [`AdditiveShare`]: crate::secret_sharing::replicated::semi_honest::AdditiveShare
pub struct Processor {
    queries: RunningQueries,
}

#[derive(thiserror::Error, Debug)]
pub enum NewQueryError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Transport(#[from] io::Error),
    #[error(transparent)]
    OneshotRecv(#[from] oneshot::error::RecvError),
}

#[derive(thiserror::Error, Debug)]
pub enum PrepareQueryError {
    #[error("This helper is the query coordinator, cannot respond to Prepare requests")]
    WrongTarget,
    #[error("Query is already running")]
    AlreadyRunning,
    #[error(transparent)]
    StateError {
        #[from]
        source: StateError,
    },
}

#[derive(thiserror::Error, Debug)]
pub enum QueryInputError {
    #[error("The query with id {0:?} does not exist")]
    NoSuchQuery(QueryId),
    #[error(transparent)]
    StateError {
        #[from]
        source: StateError,
    },
}

#[derive(thiserror::Error, Debug)]
pub enum QueryCompletionError {
    #[error("The query with id {0:?} does not exist")]
    NoSuchQuery(QueryId),
    #[error(transparent)]
    StateError {
        #[from]
        source: StateError,
    },
}


#[inline(always)]
const fn ensure_sync<T: Sync>(thing: T) -> T {
    thing
}

impl Debug for Processor {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "QueryProcessor[{:?}]", self.queries)
    }
}

impl Processor {
    fn new() -> Self {
        ensure_sync(Self { queries: RunningQueries::default() })
    }

    /// Upon receiving a new query request:
    /// * processor generates new query id
    /// * assigns roles to helpers in the ring. Helper that received new query request becomes `Role::H1` (aka coordinator).
    /// The coordinator is in theory free to choose helpers for `Role::H2` and `Role::H3` arbitrarily (aka followers), however, this is not currently exercised.
    /// * Requests Infra and Network layer to create resources for this query
    /// * sends `prepare` request that describes the query configuration (query id, query type, field type, roles -> endpoints or reverse) to followers and waits for the confirmation
    /// * records newly created query id internally and sets query state to awaiting data
    /// * returns query configuration
    ///
    /// ## Errors
    /// When other peers failed to acknowledge this query
    #[allow(clippy::missing_panics_doc)]
    pub async fn new_query<T: Into<TransportImpl>>(&self, req: QueryConfig, transport: T) -> Result<PrepareQuery, NewQueryError> {
        let query_id = QueryId;
        let handle = self.queries.handle(query_id);
        handle.set_state(QueryState::Preparing(req))?;

        let transport = transport.into();
        let id = transport.identity();
        let [right, left] = id.others();

        let roles = RoleAssignment::try_from([(id, Role::H1), (right, Role::H2), (left, Role::H3)])
            .unwrap();

        let prepare_request = PrepareQuery {
            query_id,
            config: req,
            roles: roles.clone(),
        };

        // Inform other parties about new query. If any of them rejects it, this join will fail
        try_join(
            transport.send(left, &prepare_request, stream::empty()),
            transport.send(right, &prepare_request, stream::empty())
        ).await?;

        let gateway = Gateway::new(query_id, GatewayConfig::default(), roles.clone(), transport);
        handle.set_state(QueryState::AwaitingInputs(req, gateway))?;

        Ok(prepare_request)
    }

    /// On prepare, each follower:
    /// * ensures that it is not the leader on this query
    /// * query is not registered yet
    /// * creates gateway and network
    /// * registers query
    ///
    /// ## Errors
    /// if query is already running or this helper cannot be a follower in it
    #[cfg(never)]
    pub async fn prepare(&self, req: PrepareQuery) -> Result<(), PrepareQueryError> {
        let my_role = req.roles.role(self.transport.identity());

        if my_role == Role::H1 {
            return Err(PrepareQueryError::WrongTarget);
        }
        let handle = self.queries.handle(req.query_id);
        if handle.status().is_some() {
            return Err(PrepareQueryError::AlreadyRunning);
        }

        let network = Network::new(self.transport.clone(), req.query_id, req.roles.clone());
        let gateway = Gateway::new(my_role, network, GatewayConfig::default()).await;

        handle.set_state(QueryState::AwaitingInputs(req.config, gateway))?;

        Ok(())
    }

    /// Receive inputs for the specified query. That triggers query processing
    ///
    /// ## Errors
    /// if query is not registered on this helper.
    ///
    /// ## Panics
    /// If failed to obtain an exclusive access to the query collection.
    #[cfg(never)]
    pub fn receive_inputs(&self, input: QueryInput) -> Result<(), QueryInputError> {
        let mut queries = self.queries.inner.lock().unwrap();
        match queries.entry(input.query_id) {
            Entry::Occupied(entry) => {
                let state = entry.remove();
                if let QueryState::AwaitingInputs(config, gateway) = state {
                    queries.insert(
                        input.query_id,
                        QueryState::Running(executor::start_query(
                            config,
                            gateway,
                            input.input_stream,
                        )),
                    );
                    Ok(())
                } else {
                    let error = StateError::InvalidState {
                        from: QueryStatus::from(&state),
                        to: QueryStatus::Running,
                    };
                    queries.insert(input.query_id, state);
                    Err(QueryInputError::StateError { source: error })
                }
            }
            Entry::Vacant(_) => Err(QueryInputError::NoSuchQuery(input.query_id)),
        }
    }

    pub fn status(&self, query_id: QueryId) -> Option<QueryStatus> {
        self.queries.handle(query_id).status()
    }

    /// Handle the next command from the input stream.
    ///
    /// ## Panics
    /// if command is not a query command or if the command stream is closed
    #[cfg(never)]
    pub async fn handle_next(&mut self) {
        if let Some(command) = self.command_stream.next().await {
            tracing::trace!("new command: {:?}", command);
            match command.payload {
                TransportCommand::Query(QueryCommand::Create(req, resp)) => {
                    let result = self.new_query(req).await.unwrap();
                    resp.send(result.query_id).unwrap();
                }
                TransportCommand::Query(QueryCommand::Prepare(req, resp)) => {
                    self.prepare(req).await.unwrap();
                    resp.send(()).unwrap();
                }
                TransportCommand::Query(QueryCommand::Input(query_input, resp)) => {
                    self.receive_inputs(query_input).unwrap();
                    resp.send(()).unwrap();
                }
                // TODO no tests
                TransportCommand::Query(QueryCommand::Results(query_id, resp)) => {
                    let result = self.complete(query_id).await.unwrap();
                    resp.send(result).unwrap();
                }
                TransportCommand::StepData { .. } => panic!("unexpected command: {command:?}"),
            }
        }
    }

    /// Awaits the query completion
    ///
    /// ## Errors
    /// if query is not registered on this helper.
    ///
    /// ## Panics
    /// If failed to obtain an exclusive access to the query collection.
    pub async fn complete(
        &mut self,
        query_id: QueryId,
    ) -> Result<Box<dyn ProtocolResult>, QueryCompletionError> {
        let handle = {
            let mut queries = self.queries.inner.lock().unwrap();

            match queries.remove(&query_id) {
                Some(QueryState::Running(handle)) => {
                    queries.insert(query_id, QueryState::AwaitingCompletion);
                    Ok(handle)
                }
                Some(state) => {
                    let state_error = StateError::InvalidState {
                        from: QueryStatus::from(&state),
                        to: QueryStatus::Running,
                    };
                    queries.insert(query_id, state);
                    Err(QueryCompletionError::StateError {
                        source: state_error,
                    })
                }
                None => Err(QueryCompletionError::NoSuchQuery(query_id)),
            }
        }?;

        Ok(handle.await.unwrap())
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use super::*;
    use crate::{
        ff::FieldType,
        helpers::query::QueryType,
        sync::Arc,
    };
    use futures::pin_mut;
    use futures_util::future::poll_immediate;
    use tokio::time::MissedTickBehavior::Delay;
    use crate::test_fixture::network::{DelayedTransport, InMemoryNetwork, TransportCallbacks};

    #[tokio::test]
    async fn new_query() {
        let processors = [Processor::new(), Processor::new(), Processor::new()].map(Arc::new);
        let p0 = processors[0].clone();
        let cb1 = TransportCallbacks {
            prepare_query: Box::new(move |transport, prepare_query| Box::pin(async move {
                println!("received prepare bro!");
                unimplemented!()
            })),
            ..Default::default()
        };
        let cb2 = TransportCallbacks {
            prepare_query: Box::new(move |transport, prepare_query| Box::pin(async move {
                println!("received prepare bro!");
                unimplemented!()
            })),
            ..Default::default()
        };
        let network = InMemoryNetwork::new([TransportCallbacks::default(), cb1, cb2]);
        let t0 = DelayedTransport::new(network.transport(HelperIdentity::ONE).unwrap(), 3);
        let request = QueryConfig {
            field_type: FieldType::Fp32BitPrime,
            query_type: QueryType::TestMultiply,
        };

        let processor = p0;
        let qc_future = processor.new_query(request, &t0);
        pin_mut!(qc_future);

        // poll future once to trigger query status change
        let _qc = poll_immediate(&mut qc_future).await;

        assert_eq!(Some(QueryStatus::Preparing), processor.status(QueryId));
        t0.wait().await;

        let qc = qc_future.await.unwrap();
        let expected_assignment = RoleAssignment::new(HelperIdentity::make_three());

        assert_eq!(
            PrepareQuery {
                query_id: QueryId,
                config: request,
                roles: expected_assignment,
            },
            qc
        );
        assert_eq!(Some(QueryStatus::AwaitingInputs), processor.status(QueryId));
    }

    #[cfg(never)]
    #[tokio::test]
    async fn rejects_duplicate_query_id() {
        let network = InMemoryNetwork::default();
        let processor = active_passive(network.transports()).await;
        let [t0, _, _] = network.transports();
        let request = QueryConfig {
            field_type: FieldType::Fp32BitPrime,
            query_type: QueryType::TestMultiply,
        };

        let _qc = processor.new_query(request, &t0).await.unwrap();
        assert!(matches!(
            processor.new_query(request, &t0).await,
            Err(NewQueryError::State(StateError::AlreadyRunning)),
        ));
    }

    #[cfg(never)]
    #[tokio::test]
    async fn prepare_rejected() {
        let network = InMemoryNetwork::default();
        let transport = Arc::downgrade(&network.transports[0]);
        let transport = FailingTransport::new(transport, |command| TransportError::SendFailed {
            command_name: Some(command.name()),
            query_id: command.query_id(),
            inner: "Transport failed".into(),
        });
        let processor = Processor::new(transport).await;
        let request = QueryConfig {
            field_type: FieldType::Fp32BitPrime,
            query_type: QueryType::TestMultiply,
        };

        assert!(matches!(
            processor.new_query(request).await,
            Err(NewQueryError::Transport(TransportError::SendFailed { .. }))
        ));
    }

    #[cfg(never)]
    mod prepare {
        use super::*;

        fn prepare_query(identities: [HelperIdentity; 3]) -> PrepareQuery {
            PrepareQuery {
                query_id: QueryId,
                config: QueryConfig {
                    field_type: FieldType::Fp31,
                    query_type: QueryType::TestMultiply,
                },
                roles: RoleAssignment::new(identities),
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn happy_case() {
            let network = InMemoryNetwork::default();
            let identities = HelperIdentity::make_three();
            let req = prepare_query(identities);
            let transport = network.transport(identities[1]).unwrap();
            let processor = Processor::new(transport).await;

            assert_eq!(None, processor.status(QueryId));
            processor.prepare(req).await.unwrap();
            assert_eq!(Some(QueryStatus::AwaitingInputs), processor.status(QueryId));
        }

        #[tokio::test]
        async fn rejects_if_coordinator() {
            let network = InMemoryNetwork::default();
            let identities = HelperIdentity::make_three();
            let req = prepare_query(identities);
            let transport = network.transport(identities[0]).unwrap();
            let processor = Processor::new(transport).await;

            assert!(matches!(
                processor.prepare(req).await,
                Err(PrepareQueryError::WrongTarget)
            ));
        }

        #[tokio::test]
        async fn rejects_if_query_exists() {
            let network = InMemoryNetwork::default();
            let identities = HelperIdentity::make_three();
            let req = prepare_query(identities);
            let transport = network.transport(identities[1]).unwrap();
            let processor = Processor::new(transport).await;
            processor.prepare(req.clone()).await.unwrap();
            assert!(matches!(
                processor.prepare(req).await,
                Err(PrepareQueryError::AlreadyRunning)
            ));
        }
    }

    #[cfg(never)]
    mod e2e {
        use super::*;
        use crate::{
            ff::{Field, Fp31, GaloisField, Serializable},
            helpers::{query::IpaQueryConfig, transport::ByteArrStream},
            ipa_test_input,
            protocol::{
                attribution::input::MCAggregateCreditOutputRow, ipa::IPAInputRow, BreakdownKey,
                MatchKey,
            },
            secret_sharing::{replicated::semi_honest::AdditiveShare as Replicated, IntoShares},
            sync::Weak,
            test_fixture::{
                input::GenericReportTestInput, transport::InMemoryTransport, Reconstruct,
            },
        };
        use futures_util::future::{join_all, try_join_all};
        use generic_array::GenericArray;
        use typenum::Unsigned;

        async fn make_three(network: &InMemoryNetwork) -> [Processor<Weak<InMemoryTransport>>; 3] {
            join_all(
                network
                    .transports
                    .iter()
                    .map(|transport| async { Processor::new(Arc::downgrade(transport)).await }),
            )
            .await
            .try_into()
            .unwrap()
        }

        async fn start_query(
            network: &InMemoryNetwork,
            config: QueryConfig,
        ) -> (QueryId, [Processor<Weak<InMemoryTransport>>; 3]) {
            // Helper 1 initiates the query, 2 and 3 must confirm
            let (tx, rx) = oneshot::channel();
            let mut processors = make_three(network).await;
            network.transports[0]
                .deliver(QueryCommand::Create(config, tx))
                .await;

            join_all(processors.iter_mut().map(Processor::handle_next)).await;

            let query_id = rx.await.unwrap();

            (query_id, processors)
        }

        #[tokio::test]
        async fn test_multiply() {
            const SZ: usize = <Replicated<Fp31> as Serializable>::Size::USIZE;
            let network = InMemoryNetwork::default();
            let (query_id, mut processors) = start_query(
                &network,
                QueryConfig {
                    field_type: FieldType::Fp31,
                    query_type: QueryType::TestMultiply,
                },
            )
            .await;
            let a = Fp31::from(4u128);
            let b = Fp31::from(5u128);

            let helper_shares = (a, b).share().map(|(a, b)| {
                let mut vec = vec![0u8; 2 * SZ];
                a.serialize(GenericArray::from_mut_slice(&mut vec[..SZ]));
                b.serialize(GenericArray::from_mut_slice(&mut vec[SZ..]));
                ByteArrStream::from(vec)
            });

            // at this point, all helpers must be awaiting inputs
            let mut handle_resps = Vec::with_capacity(helper_shares.len());
            for (i, input) in helper_shares.into_iter().enumerate() {
                let (tx, rx) = oneshot::channel();
                network.transports[i]
                    .deliver(QueryCommand::Input(
                        QueryInput {
                            query_id,
                            input_stream: input,
                        },
                        tx,
                    ))
                    .await;
                handle_resps.push(rx);
            }

            // process inputs and start query processing
            let mut handles = Vec::with_capacity(processors.len());
            for processor in &mut processors {
                handles.push(processor.handle_next());
            }
            join_all(handles).await;
            try_join_all(handle_resps).await.unwrap();

            let result: [_; 3] = join_all(processors.map(|mut processor| async move {
                let r = processor.complete(query_id).await.unwrap().into_bytes();
                Replicated::<Fp31>::from_byte_slice(&r).collect::<Vec<_>>()
            }))
            .await
            .try_into()
            .unwrap();

            assert_eq!(vec![Fp31::from(20u128)], result.reconstruct());
        }

        #[tokio::test]
        async fn ipa() {
            const SZ: usize = <Replicated<Fp31> as Serializable>::Size::USIZE;
            const EXPECTED: &[[u128; 2]] = &[[0, 0], [1, 2], [2, 3]];
            let network = InMemoryNetwork::default();
            let (query_id, mut processors) = start_query(
                &network,
                QueryConfig {
                    field_type: FieldType::Fp31,
                    query_type: IpaQueryConfig {
                        num_multi_bits: 3,
                        per_user_credit_cap: 3,
                        max_breakdown_key: 3,
                        attribution_window_seconds: 0,
                    }
                    .into(),
                },
            )
            .await;

            let records: Vec<GenericReportTestInput<Fp31, MatchKey, BreakdownKey>> = ipa_test_input!(
                [
                    { match_key: 12345, is_trigger_report: 0, breakdown_key: 1, trigger_value: 0 },
                    { match_key: 12345, is_trigger_report: 0, breakdown_key: 2, trigger_value: 0 },
                    { match_key: 68362, is_trigger_report: 0, breakdown_key: 1, trigger_value: 0 },
                    { match_key: 12345, is_trigger_report: 1, breakdown_key: 0, trigger_value: 5 },
                    { match_key: 68362, is_trigger_report: 1, breakdown_key: 0, trigger_value: 2 },
                ];
                (Fp31, MatchKey, BreakdownKey)
            );
            let helper_shares = records
                .share()
                .into_iter()
                .map(|shares| {
                    let data = shares
                        .into_iter()
                        .flat_map(|share: IPAInputRow<Fp31, MatchKey, BreakdownKey>| {
                            let mut buf =
                                [0u8; <IPAInputRow::<Fp31, MatchKey, BreakdownKey> as Serializable>::Size::USIZE];
                            share.serialize(GenericArray::from_mut_slice(&mut buf));

                            buf
                        })
                        .collect::<Vec<_>>();

                    ByteArrStream::from(data)
                })
                .collect::<Vec<_>>();

            for (i, input_stream) in helper_shares.into_iter().enumerate() {
                let (tx, rx) = oneshot::channel();
                network.transports[i]
                    .deliver(QueryCommand::Input(
                        QueryInput {
                            query_id,
                            input_stream,
                        },
                        tx,
                    ))
                    .await;
                processors[i].handle_next().await;
                rx.await.unwrap();
            }

            let result: [_; 3] = join_all(processors.map(|mut processor| async move {
                let r = processor.complete(query_id).await.unwrap().into_bytes();
                MCAggregateCreditOutputRow::<Fp31, Replicated<Fp31>, BreakdownKey>::from_byte_slice(
                    &r,
                )
                .collect::<Vec<_>>()
            }))
            .await
            .try_into()
            .unwrap();

            let result: Vec<GenericReportTestInput<Fp31, MatchKey, BreakdownKey>> =
                result.reconstruct();
            assert_eq!(result.len(), EXPECTED.len());
            for (i, expected) in EXPECTED.iter().enumerate() {
                assert_eq!(
                    *expected,
                    [
                        result[i].breakdown_key.as_u128(),
                        result[i].trigger_value.as_u128()
                    ]
                );
            }
        }
    }
}

#![cfg_attr(any(doc, test), doc = include_str!("../README.md"))]
#![cfg_attr(not(any(doc, test)), doc = env!("CARGO_PKG_NAME"))]
#![warn(missing_docs)]

use std::{
    pin::Pin,
    task::{self, Poll},
};

use futures::{FutureExt as _, Stream, StreamExt as _, TryFutureExt as _, future, stream};
use juniper::{
    BoxFuture, ExecutionError, ExecutionOutput, GraphQLError, GraphQLSubscriptionType,
    GraphQLTypeAsync, Object, ScalarValue, SubscriptionConnection, SubscriptionCoordinator, Value,
    ValuesStream, http::GraphQLRequest,
};

/// Simple [`SubscriptionCoordinator`] implementation:
/// - contains the schema
/// - handles subscription start
pub struct Coordinator<QueryT, MutationT, SubscriptionT, CtxT, S>
where
    QueryT: GraphQLTypeAsync<S, Context = CtxT> + Send,
    QueryT::TypeInfo: Send + Sync,
    MutationT: GraphQLTypeAsync<S, Context = CtxT> + Send,
    MutationT::TypeInfo: Send + Sync,
    SubscriptionT: GraphQLSubscriptionType<S, Context = CtxT> + Send,
    SubscriptionT::TypeInfo: Send + Sync,
    CtxT: Sync,
    S: ScalarValue + Send + Sync,
{
    root_node: juniper::RootNode<QueryT, MutationT, SubscriptionT, S>,
}

impl<QueryT, MutationT, SubscriptionT, CtxT, S>
    Coordinator<QueryT, MutationT, SubscriptionT, CtxT, S>
where
    QueryT: GraphQLTypeAsync<S, Context = CtxT> + Send,
    QueryT::TypeInfo: Send + Sync,
    MutationT: GraphQLTypeAsync<S, Context = CtxT> + Send,
    MutationT::TypeInfo: Send + Sync,
    SubscriptionT: GraphQLSubscriptionType<S, Context = CtxT> + Send,
    SubscriptionT::TypeInfo: Send + Sync,
    CtxT: Sync,
    S: ScalarValue + Send + Sync,
{
    /// Builds new [`Coordinator`] with specified `root_node`
    pub fn new(root_node: juniper::RootNode<QueryT, MutationT, SubscriptionT, S>) -> Self {
        Self { root_node }
    }
}

impl<'a, QueryT, MutationT, SubscriptionT, CtxT, S> SubscriptionCoordinator<'a, CtxT, S>
    for Coordinator<QueryT, MutationT, SubscriptionT, CtxT, S>
where
    QueryT: GraphQLTypeAsync<S, Context = CtxT> + Send,
    QueryT::TypeInfo: Send + Sync,
    MutationT: GraphQLTypeAsync<S, Context = CtxT> + Send,
    MutationT::TypeInfo: Send + Sync,
    SubscriptionT: GraphQLSubscriptionType<S, Context = CtxT> + Send,
    SubscriptionT::TypeInfo: Send + Sync,
    CtxT: Sync,
    S: ScalarValue + Send + Sync + 'a,
{
    type Connection = Connection<'a, S>;
    type Error = GraphQLError;

    fn subscribe(
        &'a self,
        req: &'a GraphQLRequest<S>,
        context: &'a CtxT,
    ) -> BoxFuture<'a, Result<Self::Connection, Self::Error>> {
        juniper::http::resolve_into_stream(req, &self.root_node, context)
            .map_ok(|(stream, errors)| Connection::from_stream(stream, errors))
            .boxed()
    }
}

/// Simple [`SubscriptionConnection`] implementation.
///
/// Resolves `Value<ValuesStream>` into `Stream<Item = ExecutionOutput<S>>` using
/// the following logic:
///
/// [`Value::Null`] - returns [`Value::Null`] once
/// [`Value::Scalar`] - returns `Ok` value or [`Value::Null`] and errors vector
/// [`Value::List`] - resolves each stream from the list using current logic and returns
///                   values in the order received
/// [`Value::Object`] - waits while each field of the [`Object`] is returned, then yields the whole object
/// `Value::Object<Value::Object<_>>` - returns [`Value::Null`] if [`Value::Object`] consists of sub-objects
pub struct Connection<'a, S> {
    stream: Pin<Box<dyn Stream<Item = ExecutionOutput<S>> + Send + 'a>>,
}

impl<'a, S> Connection<'a, S>
where
    S: ScalarValue + Send + Sync + 'a,
{
    /// Creates new [`Connection`] from values stream and errors
    pub fn from_stream(stream: Value<ValuesStream<'a, S>>, errors: Vec<ExecutionError<S>>) -> Self {
        Self {
            stream: whole_responses_stream(stream, errors),
        }
    }
}

impl<'a, S> SubscriptionConnection<S> for Connection<'a, S> where S: ScalarValue + Send + Sync + 'a {}

impl<'a, S> Stream for Connection<'a, S>
where
    S: ScalarValue + Send + Sync + 'a,
{
    type Item = ExecutionOutput<S>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

/// Creates [`futures::Stream`] that yields `ExecutionOutput<S>`s depending on the given [`Value`]:
///
/// [`Value::Null`] - returns [`Value::Null`] once
/// [`Value::Scalar`] - returns `Ok` value or [`Value::Null`] and errors vector
/// [`Value::List`] - resolves each stream from the list using current logic and returns
///                   values in the order received
/// [`Value::Object`] - waits while each field of the [`Object`] is returned, then yields the whole object
/// `Value::Object<Value::Object<_>>` - returns [`Value::Null`] if [`Value::Object`] consists of sub-objects
fn whole_responses_stream<'a, S>(
    stream: Value<ValuesStream<'a, S>>,
    errors: Vec<ExecutionError<S>>,
) -> Pin<Box<dyn Stream<Item = ExecutionOutput<S>> + Send + 'a>>
where
    S: ScalarValue + Send + Sync + 'a,
{
    if !errors.is_empty() {
        return stream::once(future::ready(ExecutionOutput {
            data: Value::null(),
            errors,
        }))
        .boxed();
    }

    match stream {
        Value::Null => Box::pin(stream::once(future::ready(ExecutionOutput::from_data(
            Value::null(),
        )))),
        Value::Scalar(s) => Box::pin(s.map(|res| match res {
            Ok(val) => ExecutionOutput::from_data(val),
            Err(err) => ExecutionOutput {
                data: Value::null(),
                errors: vec![err],
            },
        })),
        Value::List(list) => {
            let mut streams = vec![];
            for s in list.into_iter() {
                streams.push(whole_responses_stream(s, vec![]));
            }
            Box::pin(stream::select_all(streams))
        }
        Value::Object(mut object) => {
            let obj_len = object.field_count();
            if obj_len == 0 {
                return stream::once(future::ready(ExecutionOutput::from_data(Value::null())))
                    .boxed();
            }

            let mut filled_count = 0;
            let mut ready_vec = Vec::with_capacity(obj_len);
            for _ in 0..obj_len {
                ready_vec.push(None);
            }

            let stream = stream::poll_fn(move |ctx| -> Poll<Option<ExecutionOutput<S>>> {
                let mut obj_iterator = object.iter_mut();

                // Due to having to modify `ready_vec` contents (by-move pattern)
                // and only being able to iterate over `object`'s mutable references (by-ref pattern)
                // `ready_vec` and `object` cannot be iterated simultaneously.
                // TODO: iterate over i and (ref field_name, ref val) once
                //       [this RFC](https://github.com/rust-lang/rust/issues/68354)
                //       is implemented
                for ready in ready_vec.iter_mut().take(obj_len) {
                    let (field_name, val) = match obj_iterator.next() {
                        Some(v) => v,
                        None => break,
                    };

                    if ready.is_some() {
                        continue;
                    }

                    match val {
                        Value::Scalar(stream) => {
                            match Pin::new(stream).poll_next(ctx) {
                                Poll::Ready(None) => return Poll::Ready(None),
                                Poll::Ready(Some(value)) => {
                                    *ready = Some((field_name.clone(), value));
                                    filled_count += 1;
                                }
                                Poll::Pending => { /* check back later */ }
                            }
                        }
                        _ => {
                            // For now only `Object<Value::Scalar>` is supported
                            *ready = Some((field_name.clone(), Ok(Value::Null)));
                            filled_count += 1;
                        }
                    }
                }

                if filled_count == obj_len {
                    let mut errors = vec![];
                    filled_count = 0;
                    let new_vec = (0..obj_len).map(|_| None).collect::<Vec<_>>();
                    let ready_vec = std::mem::replace(&mut ready_vec, new_vec);
                    let ready_vec_iterator = ready_vec.into_iter().map(|el| {
                        let (name, val) = el.unwrap();
                        match val {
                            Ok(value) => (name, value),
                            Err(e) => {
                                errors.push(e);
                                (name, Value::Null)
                            }
                        }
                    });
                    let obj = Object::from_iter(ready_vec_iterator);
                    Poll::Ready(Some(ExecutionOutput {
                        data: Value::Object(obj),
                        errors,
                    }))
                } else {
                    Poll::Pending
                }
            });

            Box::pin(stream)
        }
    }
}

#[cfg(test)]
mod whole_responses_stream {
    use std::task::Poll;

    use futures::{StreamExt as _, stream};
    use juniper::{
        DefaultScalarValue, ExecutionError, ExecutionOutput, FieldError, Object, Value,
        ValuesStream, graphql_value,
    };

    use super::whole_responses_stream;

    #[tokio::test]
    async fn with_error() {
        let expected: Vec<ExecutionOutput<DefaultScalarValue>> = vec![ExecutionOutput {
            data: graphql_value!(null),
            errors: vec![ExecutionError::at_origin(FieldError::new(
                "field error",
                graphql_value!(null),
            ))],
        }];
        let expected = serde_json::to_string(&expected).unwrap();

        let result = whole_responses_stream::<DefaultScalarValue>(
            Value::Null,
            vec![ExecutionError::at_origin(FieldError::new(
                "field error",
                graphql_value!(null),
            ))],
        )
        .collect::<Vec<_>>()
        .await;
        let result = serde_json::to_string(&result).unwrap();

        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn value_null() {
        let expected: Vec<ExecutionOutput<DefaultScalarValue>> =
            vec![ExecutionOutput::from_data(graphql_value!(null))];
        let expected = serde_json::to_string(&expected).unwrap();

        let result = whole_responses_stream::<DefaultScalarValue>(Value::Null, vec![])
            .collect::<Vec<_>>()
            .await;
        let result = serde_json::to_string(&result).unwrap();

        assert_eq!(result, expected);
    }

    type PollResult = Result<Value<DefaultScalarValue>, ExecutionError<DefaultScalarValue>>;

    #[tokio::test]
    async fn value_scalar() {
        let expected: Vec<ExecutionOutput<DefaultScalarValue>> = vec![
            ExecutionOutput::from_data(graphql_value!(1)),
            ExecutionOutput::from_data(graphql_value!(2)),
            ExecutionOutput::from_data(graphql_value!(3)),
            ExecutionOutput::from_data(graphql_value!(4)),
            ExecutionOutput::from_data(graphql_value!(5)),
        ];
        let expected = serde_json::to_string(&expected).unwrap();

        let mut counter = 0;
        let stream = stream::poll_fn(move |_| -> Poll<Option<PollResult>> {
            if counter == 5 {
                return Poll::Ready(None);
            }
            counter += 1;
            Poll::Ready(Some(Ok(graphql_value!(counter))))
        });

        let result =
            whole_responses_stream::<DefaultScalarValue>(Value::Scalar(Box::pin(stream)), vec![])
                .collect::<Vec<_>>()
                .await;
        let result = serde_json::to_string(&result).unwrap();

        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn value_list() {
        let expected: Vec<ExecutionOutput<DefaultScalarValue>> = vec![
            ExecutionOutput::from_data(graphql_value!(1)),
            ExecutionOutput::from_data(graphql_value!(2)),
            ExecutionOutput::from_data(graphql_value!(null)),
            ExecutionOutput::from_data(graphql_value!(4)),
        ];
        let expected = serde_json::to_string(&expected).unwrap();

        let streams: Vec<Value<ValuesStream>> = vec![
            Value::Scalar(Box::pin(stream::once(async {
                PollResult::Ok(graphql_value!(1))
            }))),
            Value::Scalar(Box::pin(stream::once(async {
                PollResult::Ok(graphql_value!(2))
            }))),
            Value::Null,
            Value::Scalar(Box::pin(stream::once(async {
                PollResult::Ok(graphql_value!(4))
            }))),
        ];

        let result = whole_responses_stream::<DefaultScalarValue>(Value::List(streams), vec![])
            .collect::<Vec<_>>()
            .await;
        let result = serde_json::to_string(&result).unwrap();

        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn value_object() {
        let expected: Vec<ExecutionOutput<DefaultScalarValue>> = vec![
            ExecutionOutput::from_data(graphql_value!({"one": 1, "two": 1})),
            ExecutionOutput::from_data(graphql_value!({"one": 2, "two": 2})),
        ];
        let expected = serde_json::to_string(&expected).unwrap();

        let mut counter = 0;
        let big_stream = stream::poll_fn(move |_| -> Poll<Option<PollResult>> {
            if counter == 2 {
                return Poll::Ready(None);
            }
            counter += 1;
            Poll::Ready(Some(Ok(graphql_value!(counter))))
        });

        let mut counter = 0;
        let small_stream = stream::poll_fn(move |_| -> Poll<Option<PollResult>> {
            if counter == 2 {
                return Poll::Ready(None);
            }
            counter += 1;
            Poll::Ready(Some(Ok(graphql_value!(counter))))
        });

        let vals: Vec<(&str, Value<ValuesStream>)> = vec![
            ("one", Value::Scalar(Box::pin(big_stream))),
            ("two", Value::Scalar(Box::pin(small_stream))),
        ];

        let result = whole_responses_stream::<DefaultScalarValue>(
            Value::Object(Object::from_iter(vals.into_iter())),
            vec![],
        )
        .collect::<Vec<_>>()
        .await;
        let result = serde_json::to_string(&result).unwrap();

        assert_eq!(result, expected);
    }
}

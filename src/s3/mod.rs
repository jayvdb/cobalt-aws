//! A collection of wrappers around the [aws_sdk_s3](https://docs.rs/aws-sdk-s3/latest/aws_sdk_s3/) crate.

// Standard library imports
use std::pin::Pin;
use std::{fmt::Debug, io::Error};

// External crates
use anyhow::Result;
use aws_sdk_s3::{
    config::Builder,
    operation::{get_object::GetObjectError, list_objects_v2::ListObjectsV2Error},
    primitives::ByteStream,
    types::Object,
};
use aws_smithy_async::future::pagination_stream::{PaginationStream, TryFlatMap};
use aws_types::SdkConfig;
use bytes::Bytes;
use futures::{
    stream::Stream,
    task::{Context, Poll},
    AsyncBufRead, TryStreamExt,
};

// Internal project imports
use crate::{localstack, types::SdkError};

/// Re-export of [aws_sdk_s3::client::Client](https://docs.rs/aws-sdk-s3/latest/aws_sdk_s3/client/struct.Client.html).
///
pub use aws_sdk_s3::Client;

mod async_multipart_put_object;
mod async_put_object;
mod s3_object;
pub use async_multipart_put_object::AsyncMultipartUpload;
pub use async_put_object::AsyncPutObject;
pub use s3_object::S3Object;

/// `FuturesStreamCompatByteStream` is a compatibility layer struct designed to wrap
/// `ByteStream` from the `aws_sdk_s3`. This wrapper enables the use of `ByteStream`
/// with the `futures::Stream` trait, which is necessary for integration with libraries
/// that rely on the futures crate, such as `cobalt-aws`.
///
/// # Why
/// The `aws_sdk_s3` uses Tokio's async model and exposes streams (such as `ByteStream`)
/// that are specific to Tokio's ecosystem. However, the `cobalt-aws` library operates
/// on the futures crate's async model. `FuturesStreamCompatByteStream` bridges this gap,
/// allowing `ByteStream` to be used where a `futures::Stream` is required, ensuring
/// compatibility and interoperability between these two different async ecosystems.
#[derive(Debug, Default)]
struct FuturesStreamCompatByteStream(ByteStream);

impl From<ByteStream> for FuturesStreamCompatByteStream {
    fn from(value: ByteStream) -> Self {
        FuturesStreamCompatByteStream(value)
    }
}

impl Stream for FuturesStreamCompatByteStream {
    type Item = Result<Bytes, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0)
            .poll_next(cx)
            .map_err(std::io::Error::other)
    }
}

/// `FuturesPaginationStream` is a struct that wraps the `PaginationStream` from
/// `aws_smithy_async::future::pagination_stream`, adapting it to implement the `Stream`
/// trait from the `futures` crate.
///
/// # Why
/// `PaginationStream` in `aws_smithy_async` is designed to be runtime-agnostic and
/// does not natively implement the `futures::Stream` trait. `FuturesPaginationStream`
/// provides this implementation, making `PaginationStream` compatible with the futures-based
/// asynchronous model used in libraries like `cobalt-aws`.
///
/// This adaptation is essential in scenarios where `cobalt-aws`, which relies on the
/// `futures` crate, needs to work with the AWS SDK's pagination streams. It bridges
/// the gap between different async runtimes and libraries, ensuring smoother integration
/// and functionality in Rust async applications that rely on the futures ecosystem.
///
struct FuturesPaginiationStream<I>(PaginationStream<I>);

impl<I> From<PaginationStream<I>> for FuturesPaginiationStream<I> {
    fn from(value: PaginationStream<I>) -> Self {
        FuturesPaginiationStream(value)
    }
}

impl<I> Stream for FuturesPaginiationStream<I> {
    type Item = I;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx)
    }
}

/// Create an S3 client with LocalStack support.
///
/// # Example
///
/// ```
/// use aws_config;
/// use cobalt_aws::s3::get_client;
///
/// # tokio_test::block_on(async {
/// let shared_config = aws_config::load_from_env().await;
/// let client = get_client(&shared_config).unwrap();
/// # })
/// ```
///
/// ## LocalStack
///
/// This client supports running on [LocalStack](https://localstack.cloud/).
///
/// If you're using this client from within a Lambda function that is running on
/// LocalStack, it will automatically setup the correct endpoint.
///
/// If you're using this client from outside of LocalStack but want to communicate
/// with a LocalStack instance, then set the environment variable `LOCALSTACK_HOSTNAME`:
///
/// ```shell
/// $ export LOCALSTACK_HOSTNAME=localhost
/// ```
///
/// You can also optionally set the `EDGE_PORT` variable if you need something other
/// than the default of `4566`.
///
/// See the [LocalStack configuration docs](https://docs.localstack.cloud/localstack/configuration/) for more info.
///
/// ## Errors
///
/// An error will be returned if `LOCALSTACK_HOSTNAME` is set and a valid URI cannot be constructed.
///
#[deprecated(
    since = "0.5.0",
    note = r#"
To create a `Client` with LocalStack support use `cobalt_aws::config::load_from_env()` to create a `SdkConfig` with LocalStack support.
Then `aws_sdk_s3::Client::new(&shared_config)` to create the `Client`.
"#
)]
pub fn get_client(shared_config: &SdkConfig) -> Result<Client> {
    let mut builder = Builder::from(shared_config);
    if let Some(uri) = localstack::get_endpoint_uri()? {
        builder = builder.endpoint_url(uri.to_string()).force_path_style(true);
    }
    Ok(Client::from_conf(builder.build()))
}

/// Perform a bucket listing, returning a stream of results.
///
/// # Example
///
/// ```no_run
/// use aws_config;
/// use cobalt_aws::s3::{Client, list_objects};
/// use cobalt_aws::config::load_from_env;
/// use futures::TryStreamExt;
///
/// # tokio_test::block_on(async {
/// let shared_config = load_from_env().await.unwrap();
/// let client = Client::new(&shared_config);
/// let mut objects = list_objects(&client, "my-bucket", Some("prefix".into()));
/// while let Some(item) = objects.try_next().await.unwrap() {
///     println!("{:?}", item);
/// }
/// # })
/// ```
///
/// # Implementation details
///
/// This function uses the [ListObjectsV2](https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html)
/// API and performs pagination to ensure all objects are returned.
pub fn list_objects(
    client: &Client,
    bucket: impl Into<String>,
    prefix: Option<String>,
) -> impl Stream<Item = Result<Object, SdkError<ListObjectsV2Error>>> + Unpin {
    let req = client
        .list_objects_v2()
        .bucket(bucket)
        .set_prefix(prefix)
        .into_paginator();
    let flatend_stream = TryFlatMap::new(req.send()).flat_map(|x| x.contents.unwrap_or_default());
    FuturesPaginiationStream::from(flatend_stream)
}

/// Retrieve an object from S3 as an `AsyncBufRead`.
///
/// # Example
///
/// ```no_run
/// use aws_config;
/// use cobalt_aws::s3::{get_client, get_object};
/// use futures::AsyncReadExt;
///
/// # tokio_test::block_on(async {
/// let shared_config = aws_config::load_from_env().await;
/// let client = get_client(&shared_config).unwrap();
/// let mut reader = get_object(&client, "my-bucket", "my-key").await.unwrap();
/// let mut buffer = String::new();
/// reader.read_to_string(&mut buffer).await.unwrap();
/// println!("{}", buffer);
/// # })
/// ```
pub async fn get_object(
    client: &Client,
    bucket: &str,
    key: &str,
) -> Result<impl AsyncBufRead + Debug, SdkError<GetObjectError>> {
    let req = client.get_object().bucket(bucket).key(key);
    let resp = req.send().await?;
    Ok::<_, SdkError<GetObjectError>>(
        FuturesStreamCompatByteStream::from(resp.body).into_async_read(),
    )
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::s3::S3Object;
    use anyhow::Result;
    use aws_config;
    use aws_sdk_s3::{
        operation::create_bucket::CreateBucketError,
        types::{BucketLocationConstraint, CreateBucketConfiguration},
        Client,
    };
    use rand::distributions::{Alphanumeric, DistString};
    use rand::Rng;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;
    use serial_test::serial;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use tokio;

    #[tokio::test]
    #[serial]
    async fn test_get_client() {
        let shared_config = aws_config::load_from_env().await;
        #[allow(deprecated)]
        get_client(&shared_config).unwrap();
    }

    pub async fn create_bucket(client: &Client, bucket: &str) -> Result<()> {
        let constraint = CreateBucketConfiguration::builder()
            .location_constraint(BucketLocationConstraint::ApSoutheast2)
            .build();
        match client
            .create_bucket()
            .bucket(bucket)
            .create_bucket_configuration(constraint)
            .send()
            .await
        {
            Ok(_) => Ok::<(), anyhow::Error>(()),
            Err(e) => match e {
                SdkError::ServiceError(ref context) => match context.err() {
                    CreateBucketError::BucketAlreadyOwnedByYou(_) => Ok::<(), anyhow::Error>(()),
                    _ => Err(anyhow::Error::from(e)),
                },
                e => Err(anyhow::Error::from(e)),
            },
        }
    }

    pub fn seeded_rng<H: Hash + ?Sized>(seed: &H) -> impl Rng {
        let mut hasher = DefaultHasher::new();
        seed.hash(&mut hasher);
        ChaCha8Rng::seed_from_u64(hasher.finish())
    }

    pub fn gen_random_file_name<R: Rng>(rng: &mut R) -> String {
        Alphanumeric.sample_string(rng, 16)
    }

    pub async fn fetch_bytes(client: &Client, obj: &S3Object) -> Result<Vec<u8>> {
        Ok(client
            .get_object()
            .bucket(&obj.bucket)
            .key(&obj.key)
            .send()
            .await
            .expect("Expected dst key to exist")
            .body
            .collect()
            .await
            .expect("Expected a body")
            .into_bytes()
            .into_iter()
            .collect())
    }
}

#[cfg(test)]
mod test_list_objects {
    use super::*;
    use aws_config;
    use futures::TryStreamExt;
    use serial_test::serial;
    use std::error::Error;
    use tokio;

    async fn localstack_test_client() -> Client {
        localstack::test_utils::wait_for_localstack().await;
        let shared_config = aws_config::load_from_env().await;
        #[allow(deprecated)]
        get_client(&shared_config).unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn test_non_existant_bucket() {
        let client = localstack_test_client().await;

        let stream = list_objects(&client, "non-existant-bucket", None);
        let e = stream.try_collect::<Vec<_>>().await.unwrap_err();
        assert!(matches!(
            e.source()
                .unwrap()
                .downcast_ref::<ListObjectsV2Error>()
                .unwrap(),
            ListObjectsV2Error::NoSuchBucket(_)
        ))
    }

    #[tokio::test]
    #[serial]
    async fn test_empty_bucket() {
        let client = localstack_test_client().await;

        let stream = list_objects(&client, "empty-bucket", None);
        let results = stream.try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(results, vec![]);
    }

    #[tokio::test]
    #[serial]
    async fn test_no_prefix() {
        let client = localstack_test_client().await;

        let stream = list_objects(&client, "test-bucket", None);
        let mut results = stream.try_collect::<Vec<_>>().await.unwrap();
        results.sort_by_cached_key(|x| x.size);
        assert_eq!(results.len(), 2503);
    }

    #[tokio::test]
    #[serial]
    async fn test_with_prefix() {
        let client = localstack_test_client().await;

        let stream = list_objects(&client, "test-bucket", Some("some-prefix".into()));
        let results = stream.try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].key,
            Some("some-prefix/nested-prefix/nested.txt".into())
        );
        assert_eq!(results[0].size, Some(12));
        assert_eq!(results[1].key, Some("some-prefix/prefixed.txt".into()));
        assert_eq!(results[1].size, Some(14));
    }

    #[tokio::test]
    #[serial]
    async fn test_with_prefix_slash() {
        let client = localstack_test_client().await;

        let stream = list_objects(&client, "test-bucket", Some("some-prefix/".into()));
        let results = stream.try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].key,
            Some("some-prefix/nested-prefix/nested.txt".into())
        );
        assert_eq!(results[0].size, Some(12));
        assert_eq!(results[1].key, Some("some-prefix/prefixed.txt".into()));
        assert_eq!(results[1].size, Some(14));
    }

    #[tokio::test]
    #[serial]
    async fn test_with_nested_prefix() {
        let client = localstack_test_client().await;

        let stream = list_objects(
            &client,
            "test-bucket",
            Some("some-prefix/nested-prefix".into()),
        );
        let results = stream.try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].key,
            Some("some-prefix/nested-prefix/nested.txt".into())
        );
        assert_eq!(results[0].size, Some(12));
    }

    #[tokio::test]
    #[serial]
    async fn test_with_partial_prefix() {
        let client = localstack_test_client().await;

        let stream = list_objects(&client, "test-bucket", Some("empty-pre".into()));
        let results = stream.try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(results.len(), 0);
    }

    #[tokio::test]
    #[serial]
    async fn test_with_empty_prefix() {
        let client = localstack_test_client().await;

        let stream = list_objects(&client, "test-bucket", Some("empty-prefix".into()));
        let results = stream.try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(results.len(), 0);
    }

    #[tokio::test]
    #[serial]
    async fn test_with_multiple_pages() {
        let client = localstack_test_client().await;

        let stream = list_objects(&client, "test-bucket", Some("multi-page".into()));
        let results = stream.try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(results.len(), 2500);
    }
}
#[cfg(test)]
mod test_get_object {
    use super::*;
    use aws_config;
    use aws_sdk_s3::error::ProvideErrorMetadata;
    use futures::AsyncReadExt;
    use serial_test::serial;
    use std::error::Error;
    use tokio;

    async fn localstack_test_client() -> Client {
        localstack::test_utils::wait_for_localstack().await;
        let shared_config = aws_config::load_from_env().await;
        #[allow(deprecated)]
        get_client(&shared_config).unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn test_non_existant_bucket() {
        let client = localstack_test_client().await;
        match get_object(&client, "non-existant-bucket", "my-object").await {
            Ok(_) => panic!("Expected an error, but got Ok"),
            Err(e) => {
                let e = e
                    .source()
                    .unwrap()
                    .downcast_ref::<GetObjectError>()
                    .unwrap();

                assert_eq!(e.code(), Some("NoSuchBucket"));
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_non_existant_key() {
        let client = localstack_test_client().await;
        match get_object(&client, "test-bucket", "non-existing-object").await {
            Ok(_) => panic!("Expected an error, but got Ok"),
            Err(e) => {
                let e = e
                    .source()
                    .unwrap()
                    .downcast_ref::<GetObjectError>()
                    .unwrap();

                assert!(matches!(e, GetObjectError::NoSuchKey(_)));
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_existing_key() {
        let client = localstack_test_client().await;
        let mut reader = get_object(&client, "test-bucket", "test.txt")
            .await
            .unwrap();
        let mut buffer = String::new();
        let bytes = reader.read_to_string(&mut buffer).await.unwrap();
        assert_eq!(buffer, "test data\n");
        assert_eq!(bytes, 10);
    }
}

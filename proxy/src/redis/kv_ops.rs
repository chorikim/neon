use std::time::Duration;

use futures::FutureExt;
use redis::aio::ConnectionLike;
use redis::{Cmd, FromRedisValue, Pipeline, RedisResult};

use super::connection_with_credentials_provider::ConnectionWithCredentialsProvider;

pub struct RedisKVClient {
    client: ConnectionWithCredentialsProvider,
}

#[allow(async_fn_in_trait)]
pub trait Queryable {
    async fn query<T: FromRedisValue>(&self, conn: &mut impl ConnectionLike) -> RedisResult<T>;
}

impl Queryable for Pipeline {
    async fn query<T: FromRedisValue>(&self, conn: &mut impl ConnectionLike) -> RedisResult<T> {
        self.query_async(conn).await
    }
}

impl Queryable for Cmd {
    async fn query<T: FromRedisValue>(&self, conn: &mut impl ConnectionLike) -> RedisResult<T> {
        self.query_async(conn).await
    }
}

impl RedisKVClient {
    pub fn new(client: ConnectionWithCredentialsProvider) -> Self {
        Self { client }
    }

    pub async fn try_connect(&mut self) -> anyhow::Result<()> {
        self.client
            .connect()
            .boxed()
            .await
            .inspect_err(|e| tracing::error!("failed to connect to redis: {e}"))
    }

    pub(crate) fn credentials_refreshed(&self) -> bool {
        self.client.credentials_refreshed()
    }

    pub(crate) async fn query<T: FromRedisValue>(
        &mut self,
        q: &impl Queryable,
    ) -> anyhow::Result<T> {
        let e = match q.query(&mut self.client).await {
            Ok(t) => return Ok(t),
            Err(e) => e,
        };

        tracing::debug!("failed to run query: {e}");
        match e.retry_method() {
            redis::RetryMethod::Reconnect => {
                tracing::info!("Redis client is disconnected. Reconnecting...");
                self.try_connect().await?;
            }
            redis::RetryMethod::RetryImmediately => {}
            redis::RetryMethod::WaitAndRetry => {
                // somewhat arbitrary.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ => Err(e)?,
        }

        Ok(q.query(&mut self.client).await?)
    }
}

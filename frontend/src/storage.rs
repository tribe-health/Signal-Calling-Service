//
// Copyright 2022 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use aws_sdk_dynamodb::{
    model::{AttributeValue, Select},
    types::SdkError,
    Client, Config, Endpoint,
};
use aws_smithy_async::rt::sleep::default_async_sleep;
use aws_smithy_types::retry::RetryConfigBuilder;
use aws_types::{region::Region, Credentials};
use calling_common::Duration;
use http::Uri;
use hyper::client::HttpConnector;
use hyper::{Body, Method, Request};
use log::*;
use serde::{Deserialize, Serialize};
use serde_dynamo::{from_item, to_item};
use std::{env, path::PathBuf};
use tokio::{io::AsyncWriteExt, sync::oneshot::Receiver};

#[cfg(test)]
use mockall::{automock, predicate::*};

use crate::{
    config,
    frontend::{GroupId, UserId},
    metrics::Timer,
};

const GROUP_CONFERENCE_ID_STRING: &str = "groupConferenceId";

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct CallRecord {
    /// The group_id that the client is authorized to join and provided to the frontend
    /// by the client.
    #[serde(rename = "groupConferenceId")]
    pub group_id: GroupId,
    /// The call_id is a random id generated and sent back to the client to let it know
    /// about the specific instance of the group_id (aka era).
    #[serde(rename = "jvbConferenceId")]
    pub call_id: String,
    /// The IP of the backend Calling Server that hosts the call.
    #[serde(rename = "jvbHost")]
    pub backend_ip: String,
    /// The region of the backend Calling Server that hosts the call.
    #[serde(rename = "region")]
    pub backend_region: String,
    /// The user_id of the user that created the call.
    pub creator: UserId,
}

#[derive(thiserror::Error, Debug)]
pub enum StorageError {
    #[error(transparent)]
    UnexpectedError(#[from] anyhow::Error),
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait Storage: Sync + Send {
    /// Gets an existing call from the table matching the given group_id or returns None.
    async fn get_call_record(&self, group_id: &GroupId)
        -> Result<Option<CallRecord>, StorageError>;
    /// Adds the given call to the table but if there is already a call with the same
    /// group_id, returns that instead.
    async fn get_or_add_call_record(
        &self,
        call: CallRecord,
    ) -> Result<Option<CallRecord>, StorageError>;
    /// Removes the given call from the table as long as the call_id of the record that
    /// exists in the table is the same.
    async fn remove_call_record(
        &self,
        group_id: &GroupId,
        call_id: &str,
    ) -> Result<(), StorageError>;
    /// Returns a list of all calls in the table that are in the given region.
    async fn get_call_records_for_region(
        &self,
        region: &str,
    ) -> Result<Vec<CallRecord>, StorageError>;
}

pub struct DynamoDb {
    client: Client,
    table_name: String,
}

impl DynamoDb {
    pub async fn new(config: &'static config::Config) -> Result<(Self, IdentityFetcher)> {
        let sleep_impl =
            default_async_sleep().ok_or_else(|| anyhow!("failed to create sleep_impl"))?;

        let identity_fetcher;

        let client = match &config.storage_endpoint {
            Some(endpoint) => {
                const KEY: &str = "DUMMY_KEY";
                const PASSWORD: &str = "DUMMY_PASSWORD";

                info!("Using endpoint for DynamodDB testing: {}", endpoint);

                // Create an identity fetcher with a dummy token path, which isn't used
                // for testing.
                identity_fetcher = IdentityFetcher::new(config, "/tmp/token");

                let aws_config = Config::builder()
                    .credentials_provider(Credentials::from_keys(KEY, PASSWORD, None))
                    .endpoint_resolver(Endpoint::immutable(Uri::from_static(endpoint)))
                    .sleep_impl(sleep_impl)
                    .region(Region::new(&config.storage_region))
                    .build();
                Client::from_conf(aws_config)
            }
            _ => {
                info!(
                    "Using region for DynamodDB access: {}",
                    config.storage_region.as_str()
                );

                // Get the location of the identity token file from the environment variable,
                // the same location that the client will try to get it from for credentials.
                let identity_token_path = env::var("AWS_WEB_IDENTITY_TOKEN_FILE")?;
                identity_fetcher = IdentityFetcher::new(config, &identity_token_path);

                // Fetch an identity token once before connecting for the first time.
                identity_fetcher.fetch_token().await?;

                let retry_config = RetryConfigBuilder::new()
                    .max_attempts(4)
                    .initial_backoff(std::time::Duration::from_millis(100))
                    .build();

                let aws_config = aws_config::from_env()
                    .sleep_impl(sleep_impl)
                    .retry_config(retry_config)
                    .region(Region::new(&config.storage_region))
                    .load()
                    .await;

                Client::new(&aws_config)
            }
        };

        Ok((
            Self {
                client,
                table_name: config.storage_table.to_string(),
            },
            identity_fetcher,
        ))
    }
}

#[async_trait]
impl Storage for DynamoDb {
    async fn get_call_record(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<CallRecord>, StorageError> {
        let response = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key(
                GROUP_CONFERENCE_ID_STRING,
                AttributeValue::S(group_id.as_ref().to_string()),
            )
            .consistent_read(true)
            .send()
            .await
            .context("failed to get_item from storage")?;

        Ok(response
            .item
            .map(|item| from_item(item).context("failed to convert item to CallRecord"))
            .transpose()?)
    }

    async fn get_or_add_call_record(
        &self,
        call: CallRecord,
    ) -> Result<Option<CallRecord>, StorageError> {
        let response = self
            .client
            .put_item()
            .table_name(&self.table_name)
            .set_item(Some(
                to_item(&call).context("failed to convert CallRecord to item")?,
            ))
            // Don't overwrite the item if it already exists.
            .condition_expression("attribute_not_exists(groupConferenceId)".to_string())
            .send()
            .await;

        match response {
            Ok(_) => Ok(Some(call)),
            Err(SdkError::ServiceError { err: e, raw: _ })
                if e.is_conditional_check_failed_exception() =>
            {
                Ok(self
                    .get_call_record(&call.group_id)
                    .await
                    .context("failed to get call from storage after conditional check failed")?)
            }
            Err(err) => Err(StorageError::UnexpectedError(
                anyhow::Error::from(err)
                    .context("failed to put_item to storage for get_or_add_call_record"),
            )),
        }
    }

    async fn remove_call_record(
        &self,
        group_id: &GroupId,
        call_id: &str,
    ) -> Result<(), StorageError> {
        let response = self
            .client
            .delete_item()
            .table_name(&self.table_name)
            // Delete the item for the given key.
            .key(
                GROUP_CONFERENCE_ID_STRING,
                AttributeValue::S(group_id.as_ref().to_string()),
            )
            // But only if the given call_id matches the expected value, otherwise the
            // previous call was removed and a new one created already.
            .condition_expression("jvbConferenceId = :value".to_string())
            .expression_attribute_values(
                ":value".to_string(),
                AttributeValue::S(call_id.to_string()),
            )
            .send()
            .await;

        match response {
            Ok(_) => Ok(()),
            Err(SdkError::ServiceError { err: e, raw: _ })
                if e.is_conditional_check_failed_exception() =>
            {
                Ok(())
            }
            Err(err) => Err(StorageError::UnexpectedError(err.into())),
        }
    }

    async fn get_call_records_for_region(
        &self,
        region: &str,
    ) -> Result<Vec<CallRecord>, StorageError> {
        let response = self
            .client
            .query()
            .table_name(&self.table_name)
            .index_name("region-index")
            .key_condition_expression("#region = :value".to_string())
            .expression_attribute_names("#region".to_string(), "region".to_string())
            .expression_attribute_values(
                ":value".to_string(),
                AttributeValue::S(region.to_string()),
            )
            .consistent_read(false)
            .select(Select::AllAttributes)
            .send()
            .await
            .context("failed to query for calls in a region")?;

        if let Some(items) = response.items {
            return Ok(items
                .into_iter()
                .map(|item| from_item(item).context("failed to convert item to CallRecord"))
                .collect::<Result<_>>()?);
        }

        Ok(vec![])
    }
}

/// Supports the DynamoDB storage implementation by periodically refreshing an identity
/// token file at the location given by `identity_token_path`.
pub struct IdentityFetcher {
    client: hyper::Client<HttpConnector>,
    fetch_interval: Duration,
    identity_token_path: PathBuf,
    identity_token_url: Option<String>,
}

impl IdentityFetcher {
    fn new(config: &'static config::Config, identity_token_path: &str) -> Self {
        IdentityFetcher {
            client: hyper::client::Client::builder().build_http(),
            fetch_interval: Duration::from_millis(config.identity_fetcher_interval_ms),
            identity_token_path: PathBuf::from(identity_token_path),
            identity_token_url: config.identity_token_url.to_owned(),
        }
    }

    async fn fetch_token(&self) -> Result<()> {
        if let Some(url) = &self.identity_token_url {
            let request = Request::builder()
                .method(Method::GET)
                .uri(url)
                .header("Metadata-Flavor", "Google")
                .body(Body::empty())?;

            debug!("Fetching identity token from {}", url);

            let body = self.client.request(request).await?;
            let body = hyper::body::to_bytes(body).await?;
            let temp_name = self.identity_token_path.with_extension("bak");
            let mut temp_file = tokio::fs::File::create(&temp_name).await?;
            temp_file.write_all(&body).await?;
            tokio::fs::rename(temp_name, &self.identity_token_path).await?;

            debug!(
                "Successfully wrote identity token to {:?}",
                &self.identity_token_path
            );
        }
        Ok(())
    }

    pub async fn start(self, ender_rx: Receiver<()>) -> Result<()> {
        // Periodically fetch a new web identity from GCP.
        let fetcher_handle = tokio::spawn(async move {
            loop {
                // Use sleep() instead of interval() so that we never wait *less* than one
                // interval to do the next tick.
                tokio::time::sleep(self.fetch_interval.into()).await;

                let timer = start_timer_us!("calling.frontend.identity_fetcher.timed");

                let result = &self.fetch_token().await;
                if let Err(e) = result {
                    event!("calling.frontend.identity_fetcher.error");
                    error!("Failed to fetch identity token : {:?}", e);
                }
                timer.stop();
            }
        });

        info!("fetcher ready");

        // Wait for any task to complete and cancel the rest.
        tokio::select!(
            _ = fetcher_handle => {},
            _ = ender_rx => {},
        );

        info!("fetcher shutdown");
        Ok(())
    }
}

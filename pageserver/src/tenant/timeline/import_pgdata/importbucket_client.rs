use std::ops::Bound;
use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use postgres_ffi::{ControlFileData, PgMajorVersion};
use remote_storage::{
    Download, DownloadError, DownloadKind, DownloadOpts, GenericRemoteStorage, Listing,
    ListingObject, RemotePath, RemoteStorageConfig,
};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument};
use utils::lsn::Lsn;

use super::index_part_format;
use crate::assert_u64_eq_usize::U64IsUsize;
use crate::config::PageServerConf;

pub async fn new(
    conf: &'static PageServerConf,
    location: &index_part_format::Location,
    cancel: CancellationToken,
) -> Result<RemoteStorageWrapper, anyhow::Error> {
    // Downloads should be reasonably sized. We do ranged reads for relblock raw data
    // and full reads for SLRU segments which are bounded by Postgres.
    let timeout = RemoteStorageConfig::DEFAULT_TIMEOUT;
    let location_storage = match location {
        #[cfg(feature = "testing")]
        index_part_format::Location::LocalFs { path } => {
            GenericRemoteStorage::LocalFs(remote_storage::LocalFs::new(path.clone(), timeout)?)
        }
        index_part_format::Location::AwsS3 {
            region,
            bucket,
            key,
        } => {
            // TODO: think about security implications of letting the client specify the bucket & prefix.
            // It's the most flexible right now, but, possibly we want to move bucket name into PS conf
            // and force the timeline_id into the prefix?
            GenericRemoteStorage::AwsS3(Arc::new(
                remote_storage::S3Bucket::new(
                    &remote_storage::S3Config {
                        bucket_name: bucket.clone(),
                        prefix_in_bucket: Some(key.clone()),
                        bucket_region: region.clone(),
                        endpoint: conf
                            .import_pgdata_aws_endpoint_url
                            .clone()
                            .map(|url| url.to_string()), //  by specifying None here, remote_storage/aws-sdk-rust will infer from env
                        // This matches the default import job concurrency. This is managed
                        // separately from the usual S3 client, but the concern here is bandwidth
                        // usage.
                        concurrency_limit: 128.try_into().unwrap(),
                        max_keys_per_list_response: Some(1000),
                        upload_storage_class: None, // irrelevant
                    },
                    timeout,
                )
                .await
                .context("setup s3 bucket")?,
            ))
        }
    };
    let storage_wrapper = RemoteStorageWrapper::new(location_storage, cancel);
    Ok(storage_wrapper)
}

/// Wrap [`remote_storage`] APIs to make it look a bit more like a filesystem API
/// such as [`tokio::fs`], which was used in the original implementation of the import code.
#[derive(Clone)]
pub struct RemoteStorageWrapper {
    storage: GenericRemoteStorage,
    cancel: CancellationToken,
}

impl RemoteStorageWrapper {
    pub fn new(storage: GenericRemoteStorage, cancel: CancellationToken) -> Self {
        Self { storage, cancel }
    }

    #[instrument(level = tracing::Level::DEBUG, skip_all, fields(%path))]
    pub async fn listfilesindir(
        &self,
        path: &RemotePath,
    ) -> Result<Vec<(RemotePath, usize)>, DownloadError> {
        assert!(
            path.object_name().is_some(),
            "must specify dirname, without trailing slash"
        );
        let path = path.add_trailing_slash();

        let res = crate::tenant::remote_timeline_client::download::download_retry_forever(
            || async {
                let Listing { keys, prefixes: _ } = self
                    .storage
                    .list(
                        Some(&path),
                        remote_storage::ListingMode::WithDelimiter,
                        None,
                        &self.cancel,
                    )
                    .await?;
                let res = keys
                    .into_iter()
                    .map(|ListingObject { key, size, .. }| (key, size.into_usize()))
                    .collect();
                Ok(res)
            },
            &format!("listfilesindir {path:?}"),
            &self.cancel,
        )
        .await;
        debug!(?res, "returning");
        res
    }

    #[instrument(level = tracing::Level::DEBUG, skip_all, fields(%path))]
    pub async fn listdir(&self, path: &RemotePath) -> Result<Vec<RemotePath>, DownloadError> {
        assert!(
            path.object_name().is_some(),
            "must specify dirname, without trailing slash"
        );
        let path = path.add_trailing_slash();

        let res = crate::tenant::remote_timeline_client::download::download_retry_forever(
            || async {
                let Listing { keys, prefixes } = self
                    .storage
                    .list(
                        Some(&path),
                        remote_storage::ListingMode::WithDelimiter,
                        None,
                        &self.cancel,
                    )
                    .await?;
                let res = keys
                    .into_iter()
                    .map(|ListingObject { key, .. }| key)
                    .chain(prefixes.into_iter())
                    .collect();
                Ok(res)
            },
            &format!("listdir {path:?}"),
            &self.cancel,
        )
        .await;
        debug!(?res, "returning");
        res
    }

    #[instrument(level = tracing::Level::DEBUG, skip_all, fields(%path))]
    pub async fn get(&self, path: &RemotePath) -> Result<Bytes, DownloadError> {
        let res = crate::tenant::remote_timeline_client::download::download_retry_forever(
            || async {
                let Download {
                    download_stream, ..
                } = self
                    .storage
                    .download(path, &DownloadOpts::default(), &self.cancel)
                    .await?;
                let mut reader = tokio_util::io::StreamReader::new(download_stream);

                // XXX optimize this, can we get the capacity hint from somewhere?
                let mut buf = Vec::new();
                tokio::io::copy_buf(&mut reader, &mut buf).await?;
                Ok(Bytes::from(buf))
            },
            &format!("download {path:?}"),
            &self.cancel,
        )
        .await;
        debug!(len = res.as_ref().ok().map(|buf| buf.len()), "done");
        res
    }

    #[instrument(level = tracing::Level::DEBUG, skip_all, fields(%path))]
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &RemotePath,
    ) -> Result<Option<T>, DownloadError> {
        let buf = match self.get(path).await {
            Ok(buf) => buf,
            Err(DownloadError::NotFound) => return Ok(None),
            Err(err) => return Err(err),
        };
        let res = serde_json::from_slice(&buf)
            .context("serialize")
            // TODO: own error type
            .map_err(DownloadError::Other)?;
        Ok(Some(res))
    }

    #[instrument(level = tracing::Level::DEBUG, skip_all, fields(%path))]
    pub async fn get_range(
        &self,
        path: &RemotePath,
        start_inclusive: u64,
        end_exclusive: u64,
    ) -> Result<Vec<u8>, DownloadError> {
        let len = end_exclusive
            .checked_sub(start_inclusive)
            .unwrap()
            .into_usize();
        let res = crate::tenant::remote_timeline_client::download::download_retry_forever(
            || async {
                let Download {
                    download_stream, ..
                } = self
                    .storage
                    .download(
                        path,
                        &DownloadOpts {
                            kind: DownloadKind::Large,
                            etag: None,
                            byte_start: Bound::Included(start_inclusive),
                            byte_end: Bound::Excluded(end_exclusive),
                            version_id: None,
                        },
                        &self.cancel)
                    .await?;
                let mut reader = tokio_util::io::StreamReader::new(download_stream);

                let mut buf = Vec::with_capacity(len);
                tokio::io::copy_buf(&mut reader, &mut buf).await?;
                Ok(buf)
            },
            &format!("download range len=0x{len:x} [0x{start_inclusive:x},0x{end_exclusive:x}) from {path:?}"),
            &self.cancel,
        )
        .await;
        debug!(len = res.as_ref().ok().map(|buf| buf.len()), "done");
        res
    }

    pub fn pgdata(&self) -> RemotePath {
        RemotePath::from_string("pgdata").unwrap()
    }

    pub async fn get_control_file(&self) -> Result<ControlFile, anyhow::Error> {
        let control_file_path = self.pgdata().join("global/pg_control");
        info!("get control file from {control_file_path}");
        let control_file_buf = self.get(&control_file_path).await?;
        ControlFile::new(control_file_buf)
    }
}

pub struct ControlFile {
    control_file_data: ControlFileData,
    control_file_buf: Bytes,
}

impl ControlFile {
    pub(crate) fn new(control_file_buf: Bytes) -> Result<Self, anyhow::Error> {
        // XXX ControlFileData is version-specific, we're always using v14 here. v17 had changes.
        let control_file_data = ControlFileData::decode(&control_file_buf)?;
        let control_file = ControlFile {
            control_file_data,
            control_file_buf,
        };
        control_file.try_pg_version()?; // so that we can offer infallible pg_version()
        Ok(control_file)
    }
    pub(crate) fn base_lsn(&self) -> Lsn {
        Lsn(self.control_file_data.checkPoint).align()
    }
    pub(crate) fn pg_version(&self) -> PgMajorVersion {
        self.try_pg_version()
            .expect("prepare() checks that try_pg_version doesn't error")
    }
    pub(crate) fn control_file_data(&self) -> &ControlFileData {
        &self.control_file_data
    }
    pub(crate) fn control_file_buf(&self) -> &Bytes {
        &self.control_file_buf
    }

    fn try_pg_version(&self) -> anyhow::Result<PgMajorVersion> {
        Ok(match self.control_file_data.catalog_version_no {
            // thesea are from catversion.h
            202107181 => PgMajorVersion::PG14,
            202209061 => PgMajorVersion::PG15,
            202307071 => PgMajorVersion::PG16,
            202406281 => PgMajorVersion::PG17,
            catversion => {
                anyhow::bail!("unrecognized catalog version {catversion}")
            }
        })
    }
}

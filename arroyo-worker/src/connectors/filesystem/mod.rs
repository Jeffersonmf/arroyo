use std::{
    collections::HashMap,
    fmt::{Debug, Formatter},
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{bail, Result};
use arroyo_rpc::OperatorConfig;
use async_trait::async_trait;
use bincode::{Decode, Encode};
use futures::{stream::FuturesUnordered, Future};
use futures::{stream::StreamExt, TryStreamExt};
use object_store::{
    aws::{AmazonS3Builder, AwsCredential},
    local::LocalFileSystem,
    path::Path,
    CredentialProvider, MultipartId, ObjectStore, UploadPart,
};
use rusoto_core::credential::{DefaultCredentialsProvider, ProvideAwsCredentials};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::warn;
use typify::import_types;

import_types!(schema = "../connector-schemas/filesystem/table.json");

use arroyo_types::*;
pub mod json;
pub mod local;
pub mod parquet;
pub mod single_file;

use self::{
    json::{JsonLocalWriter, JsonWriter, PassThrough},
    local::{LocalFileSystemWriter, LocalWriter},
    parquet::{FixedSizeRecordBatchBuilder, ParquetLocalWriter, RecordBatchBufferingWriter},
};

use super::two_phase_committer::{TwoPhaseCommitter, TwoPhaseCommitterOperator};

pub struct FileSystemSink<
    K: Key,
    T: Data + Sync,
    R: MultiPartWriter<InputType = T> + Send + 'static,
> {
    sender: Sender<FileSystemMessages<T>>,
    checkpoint_receiver: Receiver<CheckpointData<T>>,
    _ts: PhantomData<(K, R)>,
}

pub type ParquetFileSystemSink<K, T, R> = FileSystemSink<
    K,
    T,
    BatchMultipartWriter<FixedSizeRecordBatchBuilder<R>, RecordBatchBufferingWriter<R>>,
>;

pub type JsonFileSystemSink<K, T> =
    FileSystemSink<K, T, BatchMultipartWriter<PassThrough<T>, JsonWriter<T>>>;

pub type LocalParquetFileSystemSink<K, T, R> = LocalFileSystemWriter<K, T, ParquetLocalWriter<R>>;

pub type LocalJsonFileSystemSink<K, T> = LocalFileSystemWriter<K, T, JsonLocalWriter>;

impl<K: Key, T: Data + Sync, V: LocalWriter<T>> LocalFileSystemWriter<K, T, V> {
    pub fn from_config(config_str: &str) -> TwoPhaseCommitterOperator<K, T, Self> {
        let config: OperatorConfig =
            serde_json::from_str(config_str).expect("Invalid config for FileSystemSink");
        let table: FileSystemTable =
            serde_json::from_value(config.table).expect("Invalid table config for FileSystemSink");
        let (_object_store, path): (Box<dyn ObjectStore>, Path) = match table.write_target.clone() {
            Destination::LocalFilesystem { local_directory } => {
                (Box::new(LocalFileSystem::new()), local_directory.into())
            }
            Destination::S3Bucket { .. } => {
                unreachable!("shouldn't be using local writer for S3");
            }
            Destination::FolderUri { path } => {
                object_store::parse_url(&url::Url::parse(&path).unwrap()).unwrap()
            }
        };
        let writer = LocalFileSystemWriter::new(path.to_string(), table);
        TwoPhaseCommitterOperator::new(writer)
    }
}

impl<K: Key, T: Data + Sync, R: MultiPartWriter<InputType = T> + Send + 'static>
    FileSystemSink<K, T, R>
{
    pub fn from_config(config_str: &str) -> TwoPhaseCommitterOperator<K, T, Self> {
        let config: OperatorConfig =
            serde_json::from_str(config_str).expect("Invalid config for FileSystemSink");
        let table: FileSystemTable =
            serde_json::from_value(config.table).expect("Invalid table config for FileSystemSink");
        let (object_store, path): (Box<dyn ObjectStore>, Path) = match table.write_target.clone() {
            Destination::LocalFilesystem { local_directory } => {
                (Box::new(LocalFileSystem::new()), local_directory.into())
            }
            Destination::S3Bucket {
                s3_bucket,
                s3_directory,
                aws_region,
            } => {
                (
                    Box::new(
                        // use default credentials
                        AmazonS3Builder::from_env()
                            .with_bucket_name(s3_bucket)
                            .with_credentials(Arc::new(S3Credentialing::try_new().unwrap()))
                            .with_region(aws_region)
                            .build()
                            .unwrap(),
                    ),
                    s3_directory.into(),
                )
            }
            Destination::FolderUri { path } => {
                object_store::parse_url(&url::Url::parse(&path).unwrap()).unwrap()
            }
        };

        let (sender, receiver) = tokio::sync::mpsc::channel(10000);
        let (checkpoint_sender, checkpoint_receiver) = tokio::sync::mpsc::channel(10000);
        let mut writer = AsyncMultipartFileSystemWriter::<T, R>::new(
            path,
            Arc::new(object_store),
            receiver,
            checkpoint_sender,
            table,
        );
        tokio::spawn(async move {
            writer.run().await.unwrap();
        });
        TwoPhaseCommitterOperator::new(Self {
            sender,
            checkpoint_receiver,
            _ts: PhantomData,
        })
    }
}

#[derive(Debug)]
enum FileSystemMessages<T: Data> {
    Data {
        value: T,
        time: SystemTime,
    },
    Init {
        max_file_index: usize,
        subtask_id: usize,
        recovered_files: Vec<InProgressFileCheckpoint<T>>,
    },
    Checkpoint {
        subtask_id: usize,
        then_stop: bool,
    },
    FilesToFinish(Vec<FileToFinish>),
}

#[derive(Debug)]
enum CheckpointData<T: Data> {
    InProgressFileCheckpoint(InProgressFileCheckpoint<T>),
    Finished { max_file_index: usize },
}

#[derive(Debug, Decode, Encode, Clone, PartialEq, Eq)]
struct InProgressFileCheckpoint<T: Data> {
    filename: String,
    data: FileCheckpointData,
    buffered_data: Vec<T>,
}

#[derive(Debug, Decode, Encode, Clone, PartialEq, Eq)]
pub enum FileCheckpointData {
    Empty,
    MultiPartNotCreated {
        parts_to_add: Vec<Vec<u8>>,
        trailing_bytes: Option<Vec<u8>>,
    },
    MultiPartInFlight {
        multi_part_upload_id: String,
        in_flight_parts: Vec<InFlightPartCheckpoint>,
        trailing_bytes: Option<Vec<u8>>,
    },
    MultiPartWriterClosed {
        multi_part_upload_id: String,
        in_flight_parts: Vec<InFlightPartCheckpoint>,
    },
    MultiPartWriterUploadCompleted {
        multi_part_upload_id: String,
        completed_parts: Vec<String>,
    },
}

#[derive(Debug, Decode, Encode, Clone, PartialEq, Eq)]
pub enum InFlightPartCheckpoint {
    FinishedPart { part: usize, content_id: String },
    InProgressPart { part: usize, data: Vec<u8> },
}

struct S3Credentialing {
    credentials_provider: DefaultCredentialsProvider,
}

impl Debug for S3Credentialing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentialing").finish()
    }
}

impl S3Credentialing {
    fn try_new() -> Result<Self> {
        Ok(Self {
            credentials_provider: DefaultCredentialsProvider::new()?,
        })
    }
}

#[async_trait::async_trait]
impl CredentialProvider for S3Credentialing {
    #[doc = " The type of credential returned by this provider"]
    type Credential = AwsCredential;

    /// Return a credential
    async fn get_credential(&self) -> object_store::Result<Arc<Self::Credential>> {
        let credentials = self
            .credentials_provider
            .credentials()
            .await
            .map_err(|err| object_store::Error::Generic {
                store: "s3",
                source: Box::new(err),
            })?;
        Ok(Arc::new(AwsCredential {
            key_id: credentials.aws_access_key_id().to_string(),
            secret_key: credentials.aws_secret_access_key().to_string(),
            token: credentials.token().clone(),
        }))
    }
}

struct AsyncMultipartFileSystemWriter<T: Data + Sync, R: MultiPartWriter> {
    path: Path,
    current_writer_name: String,
    max_file_index: usize,
    subtask_id: usize,
    object_store: Arc<dyn ObjectStore>,
    writers: HashMap<String, R>,
    receiver: Receiver<FileSystemMessages<T>>,
    checkpoint_sender: Sender<CheckpointData<T>>,
    futures: FuturesUnordered<BoxedTryFuture<MultipartCallbackWithName>>,
    files_to_finish: Vec<FileToFinish>,
    properties: FileSystemTable,
    rolling_policy: RollingPolicy,
}

#[async_trait]
pub trait MultiPartWriter {
    type InputType: Data;
    fn new(object_store: Arc<dyn ObjectStore>, path: Path, config: &FileSystemTable) -> Self;

    fn name(&self) -> String;

    async fn insert_value(
        &mut self,
        value: Self::InputType,
        time: SystemTime,
    ) -> Result<Option<BoxedTryFuture<MultipartCallbackWithName>>>;

    fn handle_initialization(
        &mut self,
        multipart_id: String,
    ) -> Result<Vec<BoxedTryFuture<MultipartCallbackWithName>>>;

    fn handle_completed_part(
        &mut self,
        part_idx: usize,
        upload_part: UploadPart,
    ) -> Result<Option<FileToFinish>>;

    fn get_in_progress_checkpoint(&mut self) -> FileCheckpointData;

    fn currently_buffered_data(&mut self) -> Vec<Self::InputType>;

    fn close(&mut self) -> Result<Option<BoxedTryFuture<MultipartCallbackWithName>>>;

    fn stats(&self) -> Option<MultiPartWriterStats>;

    fn get_finished_file(&mut self) -> FileToFinish;
}

async fn from_checkpoint(
    path: &Path,
    checkpoint_data: FileCheckpointData,
    object_store: Arc<dyn ObjectStore>,
) -> Result<Option<FileToFinish>> {
    let mut parts = vec![];
    let multipart_id = match checkpoint_data {
        FileCheckpointData::Empty => {
            return Ok(None);
        }
        FileCheckpointData::MultiPartNotCreated {
            parts_to_add,
            trailing_bytes,
        } => {
            let multipart_id = object_store
                .start_multipart(path)
                .await
                .expect("failed to create multipart upload");
            let mut parts = vec![];
            for (part_index, data) in parts_to_add.into_iter().enumerate() {
                let upload_part = object_store
                    .add_multipart(path, &multipart_id, part_index, data.into())
                    .await
                    .unwrap();
                parts.push(upload_part);
            }
            if let Some(trailing_bytes) = trailing_bytes {
                let upload_part = object_store
                    .add_multipart(path, &multipart_id, parts.len(), trailing_bytes.into())
                    .await?;
                parts.push(upload_part);
            }
            multipart_id
        }
        FileCheckpointData::MultiPartInFlight {
            multi_part_upload_id,
            in_flight_parts,
            trailing_bytes,
        } => {
            for data in in_flight_parts.into_iter() {
                match data {
                    InFlightPartCheckpoint::FinishedPart {
                        part: _,
                        content_id,
                    } => parts.push(UploadPart { content_id }),
                    InFlightPartCheckpoint::InProgressPart { part, data } => {
                        let upload_part = object_store
                            .add_multipart(path, &multi_part_upload_id, part, data.into())
                            .await
                            .unwrap();
                        parts.push(upload_part);
                    }
                }
            }
            if let Some(trailing_bytes) = trailing_bytes {
                let upload_part = object_store
                    .add_multipart(
                        path,
                        &multi_part_upload_id,
                        parts.len(),
                        trailing_bytes.into(),
                    )
                    .await?;
                parts.push(upload_part);
            }
            multi_part_upload_id
        }
        FileCheckpointData::MultiPartWriterClosed {
            multi_part_upload_id,
            in_flight_parts,
        } => {
            for (part_index, data) in in_flight_parts.into_iter().enumerate() {
                match data {
                    InFlightPartCheckpoint::FinishedPart {
                        part: _,
                        content_id,
                    } => parts.push(UploadPart { content_id }),
                    InFlightPartCheckpoint::InProgressPart { part: _, data } => {
                        let upload_part = object_store
                            .add_multipart(path, &multi_part_upload_id, part_index, data.into())
                            .await
                            .unwrap();
                        parts.push(upload_part);
                    }
                }
            }
            multi_part_upload_id
        }
        FileCheckpointData::MultiPartWriterUploadCompleted {
            multi_part_upload_id,
            completed_parts,
        } => {
            for content_id in completed_parts {
                parts.push(UploadPart { content_id })
            }
            multi_part_upload_id
        }
    };
    Ok(Some(FileToFinish {
        filename: path.to_string(),
        multi_part_upload_id: multipart_id,
        completed_parts: parts.into_iter().map(|p| p.content_id).collect(),
    }))
}

#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
pub struct FileToFinish {
    filename: String,
    multi_part_upload_id: String,
    completed_parts: Vec<String>,
}

enum RollingPolicy {
    PartLimit(usize),
    SizeLimit(usize),
    InactivityDuration(Duration),
    RolloverDuration(Duration),
    AnyPolicy(Vec<RollingPolicy>),
}

impl RollingPolicy {
    fn should_roll(&self, stats: &MultiPartWriterStats) -> bool {
        match self {
            RollingPolicy::PartLimit(part_limit) => stats.parts_written >= *part_limit,
            RollingPolicy::SizeLimit(size_limit) => stats.bytes_written >= *size_limit,
            RollingPolicy::InactivityDuration(duration) => {
                stats.last_write_at.elapsed() >= *duration
            }
            RollingPolicy::RolloverDuration(duration) => {
                stats.first_write_at.elapsed() >= *duration
            }
            RollingPolicy::AnyPolicy(policies) => {
                policies.iter().any(|policy| policy.should_roll(stats))
            }
        }
    }

    fn from_file_settings(file_settings: &FileSettings) -> RollingPolicy {
        let mut policies = vec![];
        let part_size_limit = file_settings.max_parts.unwrap_or(1000) as usize;
        // this is a hard limit, so will always be present.
        policies.push(RollingPolicy::PartLimit(part_size_limit));
        if let Some(file_size_target) = file_settings.target_file_size {
            policies.push(RollingPolicy::SizeLimit(file_size_target as usize))
        }
        if let Some(inactivity_timeout) = file_settings
            .inactivity_rollover_seconds
            .map(|seconds| Duration::from_secs(seconds as u64))
        {
            policies.push(RollingPolicy::InactivityDuration(inactivity_timeout))
        }
        let rollover_timeout =
            Duration::from_secs(file_settings.rollover_seconds.unwrap_or(30) as u64);
        policies.push(RollingPolicy::RolloverDuration(rollover_timeout));
        RollingPolicy::AnyPolicy(policies)
    }
}

#[derive(Debug, Clone)]
pub struct MultiPartWriterStats {
    bytes_written: usize,
    parts_written: usize,
    last_write_at: Instant,
    first_write_at: Instant,
}

impl<T, R> AsyncMultipartFileSystemWriter<T, R>
where
    T: Data + std::marker::Sync,
    R: MultiPartWriter<InputType = T>,
{
    fn new(
        path: Path,
        object_store: Arc<dyn ObjectStore>,
        receiver: Receiver<FileSystemMessages<T>>,
        checkpoint_sender: Sender<CheckpointData<T>>,
        writer_properties: FileSystemTable,
    ) -> Self {
        Self {
            path,
            current_writer_name: "".to_string(),
            max_file_index: 0,
            subtask_id: 0,
            object_store,
            writers: HashMap::new(),
            receiver,
            checkpoint_sender,
            futures: FuturesUnordered::new(),
            files_to_finish: Vec::new(),
            rolling_policy: RollingPolicy::from_file_settings(
                writer_properties.file_settings.as_ref().unwrap(),
            ),
            properties: writer_properties,
        }
    }

    fn add_part_to_finish(&mut self, file_to_finish: FileToFinish) {
        self.files_to_finish.push(file_to_finish);
    }

    async fn run(&mut self) -> Result<()> {
        let mut next_policy_check = tokio::time::Instant::now();
        loop {
            tokio::select! {
                Some(message) = self.receiver.recv() => {
                    match message {
                        FileSystemMessages::Data{value, time} => {
                            let Some(writer) = self.writers.get_mut(&self.current_writer_name) else {
                                bail!("expect the current writer to be initialized");
                            };
                            if let Some(future) = writer.insert_value(value, time).await? {
                                self.futures.push(future);
                            }
                        },
                        FileSystemMessages::Init {max_file_index, subtask_id, recovered_files } => {
                            if let Some(writer) = self.writers.get_mut(&self.current_writer_name) {
                                if let Some(future) = writer.close()? {
                                    self.futures.push(future);
                                }
                            }
                            self.max_file_index = max_file_index;
                            self.subtask_id = subtask_id;
                            let new_writer = self.new_writer();
                            self.current_writer_name = new_writer.name();
                            self.writers.insert(new_writer.name(), new_writer);
                            for recovered_file in recovered_files {
                                if let Some(file_to_finish) = from_checkpoint(
                                     &Path::parse(&recovered_file.filename)?, recovered_file.data, self.object_store.clone()).await? {
                                        self.add_part_to_finish(file_to_finish);
                                     }

                                for value in recovered_file.buffered_data {
                                    let Some(writer) = self.writers.get_mut(&self.current_writer_name) else {
                                        bail!("expect the current writer to be initialized");
                                    };
                                    if let Some(future) = writer.insert_value(value, SystemTime::now()).await? {
                                        self.futures.push(future);
                                    }
                                }
                            }
                        },
                        FileSystemMessages::Checkpoint { subtask_id, then_stop } => {
                            self.flush_futures().await?;
                            if then_stop {
                                self.stop().await?;
                            }
                            self.take_checkpoint( subtask_id).await?;
                            self.checkpoint_sender.send(CheckpointData::Finished {  max_file_index: self.max_file_index}).await?;
                        },
                        FileSystemMessages::FilesToFinish(files_to_finish) =>{
                            for file_to_finish in files_to_finish {
                                self.finish_file(file_to_finish).await?;
                            }
                            self.checkpoint_sender.send(CheckpointData::Finished {  max_file_index: self.max_file_index}).await?;
                        }
                    }
                }
                Some(result) = self.futures.next() => {
                    let MultipartCallbackWithName { callback, name } = result?;
                    self.process_callback(name, callback)?;
                }
                _ = tokio::time::sleep_until(next_policy_check) => {
                    next_policy_check = tokio::time::Instant::now() + Duration::from_millis(100);
                    if let Some(writer) = self.writers.get_mut(&self.current_writer_name) {
                        if let Some(stats) = writer.stats() {
                            if self.rolling_policy.should_roll(&stats){
                            if let Some(future) = writer.close()? {
                                self.futures.push(future);
                            }
                            self.max_file_index += 1;
                            let new_writer = self.new_writer();
                            self.current_writer_name = new_writer.name();
                            self.writers.insert(new_writer.name(), new_writer);
                        }
                    }
                    }
                }
                else => {
                    break;
                }
            }
        }
        Ok(())
    }

    fn new_writer(&mut self) -> R {
        R::new(
            self.object_store.clone(),
            format!(
                "{}/{:0>5}-{:0>3}",
                self.path, self.max_file_index, self.subtask_id
            )
            .into(),
            &self.properties,
        )
    }

    async fn flush_futures(&mut self) -> Result<()> {
        while let Some(MultipartCallbackWithName { callback, name }) =
            self.futures.try_next().await?
        {
            self.process_callback(name, callback)?;
        }
        Ok(())
    }

    fn process_callback(&mut self, name: String, callback: MultipartCallback) -> Result<()> {
        let writer = self.writers.get_mut(&name).ok_or_else(|| {
            anyhow::anyhow!("missing writer {} for callback {:?}", name, callback)
        })?;
        match callback {
            MultipartCallback::InitializedMultipart { multipart_id } => {
                self.futures
                    .extend(writer.handle_initialization(multipart_id)?);
                Ok(())
            }
            MultipartCallback::CompletedPart {
                part_idx,
                upload_part,
            } => {
                if let Some(file_to_write) = writer.handle_completed_part(part_idx, upload_part)? {
                    // need the file to finish to be checkpointed first.
                    self.add_part_to_finish(file_to_write);
                    self.writers.remove(&name);
                }
                Ok(())
            }
            MultipartCallback::UploadsFinished => {
                let file_to_write = writer.get_finished_file();
                self.add_part_to_finish(file_to_write);
                self.writers.remove(&name);
                Ok(())
            }
        }
    }

    async fn finish_file(&mut self, file_to_finish: FileToFinish) -> Result<()> {
        let FileToFinish {
            filename,
            multi_part_upload_id,
            completed_parts,
        } = file_to_finish;
        if completed_parts.len() == 0 {
            warn!("no parts to finish for file {}", filename);
            return Ok(());
        }
        let parts: Vec<_> = completed_parts
            .into_iter()
            .map(|content_id| UploadPart {
                content_id: content_id.clone(),
            })
            .collect();
        let location = Path::parse(&filename)?;
        self.object_store
            .close_multipart(&location, &multi_part_upload_id, parts)
            .await
            .unwrap();
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(writer) = self.writers.get_mut(&self.current_writer_name) {
            let close_future: Option<BoxedTryFuture<MultipartCallbackWithName>> = writer.close()?;
            if let Some(future) = close_future {
                self.futures.push(future);
            }
        }
        while let Some(result) = self.futures.next().await {
            let MultipartCallbackWithName { callback, name } = result?;
            self.process_callback(name, callback)?;
        }
        Ok(())
    }

    async fn take_checkpoint(&mut self, _subtask_id: usize) -> Result<()> {
        for (filename, writer) in self.writers.iter_mut() {
            let buffered_data = writer.currently_buffered_data();
            let in_progress_checkpoint =
                CheckpointData::InProgressFileCheckpoint(InProgressFileCheckpoint {
                    filename: filename.clone(),
                    data: writer.get_in_progress_checkpoint(),
                    buffered_data,
                });
            self.checkpoint_sender.send(in_progress_checkpoint).await?;
        }
        for file_to_finish in &self.files_to_finish {
            self.checkpoint_sender
                .send(CheckpointData::InProgressFileCheckpoint(
                    InProgressFileCheckpoint {
                        filename: file_to_finish.filename.clone(),
                        data: FileCheckpointData::MultiPartWriterUploadCompleted {
                            multi_part_upload_id: file_to_finish.multi_part_upload_id.clone(),
                            completed_parts: file_to_finish.completed_parts.clone(),
                        },
                        buffered_data: vec![],
                    },
                ))
                .await?;
        }
        self.files_to_finish.clear();
        Ok(())
    }
}

type BoxedTryFuture<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

struct MultipartManager {
    object_store: Arc<dyn ObjectStore>,
    location: Path,
    multipart_id: Option<MultipartId>,
    pushed_parts: Vec<UploadPartOrBufferedData>,
    uploaded_parts: usize,
    pushed_size: usize,
    parts_to_add: Vec<PartToUpload>,
    closed: bool,
}

impl MultipartManager {
    fn new(object_store: Arc<dyn ObjectStore>, location: Path) -> Self {
        Self {
            object_store,
            location,
            multipart_id: None,
            pushed_parts: vec![],
            uploaded_parts: 0,
            pushed_size: 0,
            parts_to_add: vec![],
            closed: false,
        }
    }

    fn name(&self) -> String {
        self.location.to_string()
    }

    fn write_next_part(
        &mut self,
        data: Vec<u8>,
    ) -> Result<Option<BoxedTryFuture<MultipartCallbackWithName>>> {
        match &self.multipart_id {
            Some(_multipart_id) => Ok(Some(self.get_part_upload_future(PartToUpload {
                part_index: self.pushed_parts.len(),
                byte_data: data,
            })?)),
            None => {
                let is_first_part = self.parts_to_add.is_empty();
                self.parts_to_add.push(PartToUpload {
                    byte_data: data,
                    part_index: self.parts_to_add.len(),
                });
                if is_first_part {
                    // start a new multipart upload
                    Ok(Some(self.get_initialize_multipart_future()?))
                } else {
                    Ok(None)
                }
            }
        }
    }
    // Future for uploading a part of a multipart upload.
    // Argument is either from a newly flushed part or from parts_to_add.
    fn get_part_upload_future(
        &mut self,
        part_to_upload: PartToUpload,
    ) -> Result<BoxedTryFuture<MultipartCallbackWithName>> {
        self.pushed_parts
            .push(UploadPartOrBufferedData::BufferedData {
                // TODO: use Bytes to avoid clone
                data: part_to_upload.byte_data.clone(),
            });
        let location = self.location.clone();
        let multipart_id = self
            .multipart_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing multipart id"))?;
        let object_store = self.object_store.clone();
        Ok(Box::pin(async move {
            let upload_part = object_store
                .add_multipart(
                    &location,
                    &multipart_id,
                    part_to_upload.part_index,
                    part_to_upload.byte_data.into(),
                )
                .await?;
            Ok(MultipartCallbackWithName {
                name: location.to_string(),
                callback: MultipartCallback::CompletedPart {
                    part_idx: part_to_upload.part_index,
                    upload_part,
                },
            })
        }))
    }

    fn get_initialize_multipart_future(
        &mut self,
    ) -> Result<BoxedTryFuture<MultipartCallbackWithName>> {
        let object_store = self.object_store.clone();
        let location = self.location.clone();
        Ok(Box::pin(async move {
            let multipart_id = object_store.start_multipart(&location).await?;
            Ok(MultipartCallbackWithName {
                name: location.to_string(),
                callback: MultipartCallback::InitializedMultipart { multipart_id },
            })
        }))
    }

    fn handle_initialization(
        &mut self,
        multipart_id: String,
    ) -> Result<Vec<BoxedTryFuture<MultipartCallbackWithName>>> {
        // for each part in parts_to_add, start a new part upload
        self.multipart_id = Some(multipart_id);
        std::mem::take(&mut self.parts_to_add)
            .into_iter()
            .map(|part_to_upload| self.get_part_upload_future(part_to_upload))
            .collect::<Result<Vec<_>>>()
    }

    fn handle_completed_part(
        &mut self,
        part_idx: usize,
        upload_part: UploadPart,
    ) -> Result<Option<FileToFinish>> {
        self.pushed_parts[part_idx] = UploadPartOrBufferedData::UploadPart(upload_part);
        self.uploaded_parts += 1;

        if !self.all_uploads_finished() {
            Ok(None)
        } else {
            Ok(Some(FileToFinish {
                filename: self.name(),
                multi_part_upload_id: self
                    .multipart_id
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("need multipart id to complete"))?
                    .clone(),
                completed_parts: self
                    .pushed_parts
                    .iter()
                    .map(|part| match part {
                        UploadPartOrBufferedData::UploadPart(upload_part) => {
                            Ok(upload_part.content_id.clone())
                        }
                        UploadPartOrBufferedData::BufferedData { .. } => {
                            bail!("unfinished part in get_complete_multipart_future")
                        }
                    })
                    .collect::<Result<Vec<_>>>()?,
            }))
        }
    }
    fn all_uploads_finished(&self) -> bool {
        self.closed && self.uploaded_parts == self.pushed_parts.len()
    }

    fn get_closed_file_checkpoint_data(&mut self) -> FileCheckpointData {
        if !self.closed {
            unreachable!("get_closed_file_checkpoint_data called on open file");
        }
        let Some(ref multipart_id) = self.multipart_id else {
            if self.pushed_size == 0 {
                return FileCheckpointData::Empty;
            } else {
                return FileCheckpointData::MultiPartNotCreated {
                    parts_to_add: self
                        .parts_to_add
                        .iter()
                        .map(|val| val.byte_data.clone())
                        .collect(),
                    trailing_bytes: None,
                };
            }
        };
        if self.all_uploads_finished() {
            return FileCheckpointData::MultiPartWriterUploadCompleted {
                multi_part_upload_id: multipart_id.clone(),
                completed_parts: self
                    .pushed_parts
                    .iter()
                    .map(|val| match val {
                        UploadPartOrBufferedData::UploadPart(upload_part) => {
                            upload_part.content_id.clone()
                        }
                        UploadPartOrBufferedData::BufferedData { .. } => {
                            unreachable!("unfinished part in get_closed_file_checkpoint_data")
                        }
                    })
                    .collect(),
            };
        } else {
            let in_flight_parts = self
                .pushed_parts
                .iter()
                .enumerate()
                .map(|(part_index, part)| match part {
                    UploadPartOrBufferedData::UploadPart(upload_part) => {
                        InFlightPartCheckpoint::FinishedPart {
                            part: part_index,
                            content_id: upload_part.content_id.clone(),
                        }
                    }
                    UploadPartOrBufferedData::BufferedData { data } => {
                        InFlightPartCheckpoint::InProgressPart {
                            part: part_index,
                            data: data.clone(),
                        }
                    }
                })
                .collect();
            FileCheckpointData::MultiPartWriterClosed {
                multi_part_upload_id: multipart_id.clone(),
                in_flight_parts,
            }
        }
    }

    fn get_in_progress_checkpoint(
        &mut self,
        trailing_bytes: Option<Vec<u8>>,
    ) -> FileCheckpointData {
        if self.closed {
            unreachable!("get_in_progress_checkpoint called on closed file");
        }
        if self.multipart_id.is_none() {
            return FileCheckpointData::MultiPartNotCreated {
                parts_to_add: self
                    .parts_to_add
                    .iter()
                    .map(|val| val.byte_data.clone())
                    .collect(),
                trailing_bytes,
            };
        }
        let multi_part_id = self.multipart_id.as_ref().unwrap().clone();
        let in_flight_parts = self
            .pushed_parts
            .iter()
            .enumerate()
            .map(|(part_index, part)| match part {
                UploadPartOrBufferedData::UploadPart(upload_part) => {
                    InFlightPartCheckpoint::FinishedPart {
                        part: part_index,
                        content_id: upload_part.content_id.clone(),
                    }
                }
                UploadPartOrBufferedData::BufferedData { data } => {
                    InFlightPartCheckpoint::InProgressPart {
                        part: part_index,
                        data: data.clone(),
                    }
                }
            })
            .collect();
        FileCheckpointData::MultiPartInFlight {
            multi_part_upload_id: multi_part_id,
            in_flight_parts,
            trailing_bytes,
        }
    }

    fn get_finished_file(&mut self) -> FileToFinish {
        if !self.closed {
            unreachable!("get_finished_file called on open file");
        }
        FileToFinish {
            filename: self.name(),
            multi_part_upload_id: self
                .multipart_id
                .as_ref()
                .expect("finished files should have a multipart ID")
                .clone(),
            completed_parts: self
                .pushed_parts
                .iter()
                .map(|part| match part {
                    UploadPartOrBufferedData::UploadPart(upload_part) => {
                        upload_part.content_id.clone()
                    }
                    UploadPartOrBufferedData::BufferedData { .. } => {
                        unreachable!("unfinished part in get_finished_file")
                    }
                })
                .collect(),
        }
    }
}

pub trait BatchBuilder: Send {
    type InputType: Data;
    type BatchData;
    fn new(config: &FileSystemTable) -> Self;
    fn insert(&mut self, value: Self::InputType) -> Option<Self::BatchData>;
    fn buffered_inputs(&self) -> Vec<Self::InputType>;
    fn flush_buffer(&mut self) -> Self::BatchData;
}

pub trait BatchBufferingWriter: Send {
    type BatchData;
    fn new(config: &FileSystemTable) -> Self;
    fn suffix() -> String;
    fn add_batch_data(&mut self, data: Self::BatchData) -> Option<Vec<u8>>;
    fn buffer_length(&self) -> usize;
    fn evict_current_buffer(&mut self) -> Vec<u8>;
    fn get_trailing_bytes_for_checkpoint(&mut self) -> Option<Vec<u8>>;
    fn close(&mut self, final_batch: Option<Self::BatchData>) -> Option<Vec<u8>>;
}

pub struct BatchMultipartWriter<
    BB: BatchBuilder,
    BBW: BatchBufferingWriter<BatchData = BB::BatchData>,
> {
    batch_builder: BB,
    batch_buffering_writer: BBW,
    multipart_manager: MultipartManager,
    stats: Option<MultiPartWriterStats>,
}
#[async_trait]
impl<BB: BatchBuilder, BBW: BatchBufferingWriter<BatchData = BB::BatchData>> MultiPartWriter
    for BatchMultipartWriter<BB, BBW>
{
    type InputType = BB::InputType;

    fn new(object_store: Arc<dyn ObjectStore>, path: Path, config: &FileSystemTable) -> Self {
        let batch_builder = BB::new(config);
        let batch_buffering_writer = BBW::new(config);
        let path = format!("{}.{}", path, BBW::suffix()).into();
        Self {
            batch_builder,
            batch_buffering_writer,
            multipart_manager: MultipartManager::new(object_store, path),
            stats: None,
        }
    }

    fn name(&self) -> String {
        self.multipart_manager.name()
    }

    async fn insert_value(
        &mut self,
        value: Self::InputType,
        _time: SystemTime,
    ) -> Result<Option<BoxedTryFuture<MultipartCallbackWithName>>> {
        if self.stats.is_none() {
            self.stats = Some(MultiPartWriterStats {
                bytes_written: 0,
                parts_written: 0,
                last_write_at: Instant::now(),
                first_write_at: Instant::now(),
            });
        }
        let stats = self.stats.as_mut().unwrap();
        stats.last_write_at = Instant::now();

        if let Some(batch) = self.batch_builder.insert(value.clone()) {
            let prev_size = self.batch_buffering_writer.buffer_length();
            if let Some(bytes) = self.batch_buffering_writer.add_batch_data(batch) {
                stats.bytes_written += bytes.len() - prev_size;
                stats.parts_written += 1;
                self.multipart_manager.write_next_part(bytes)
            } else {
                stats.bytes_written += self.batch_buffering_writer.buffer_length() - prev_size;
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn handle_initialization(
        &mut self,
        multipart_id: String,
    ) -> Result<Vec<BoxedTryFuture<MultipartCallbackWithName>>> {
        self.multipart_manager.handle_initialization(multipart_id)
    }

    fn handle_completed_part(
        &mut self,
        part_idx: usize,
        upload_part: UploadPart,
    ) -> Result<Option<FileToFinish>> {
        self.multipart_manager
            .handle_completed_part(part_idx, upload_part)
    }

    fn get_in_progress_checkpoint(&mut self) -> FileCheckpointData {
        if self.multipart_manager.closed {
            self.multipart_manager.get_closed_file_checkpoint_data()
        } else {
            self.multipart_manager.get_in_progress_checkpoint(
                self.batch_buffering_writer
                    .get_trailing_bytes_for_checkpoint(),
            )
        }
    }

    fn currently_buffered_data(&mut self) -> Vec<Self::InputType> {
        self.batch_builder.buffered_inputs()
    }

    fn close(&mut self) -> Result<Option<BoxedTryFuture<MultipartCallbackWithName>>> {
        self.multipart_manager.closed = true;
        self.write_closing_multipart()
    }

    fn stats(&self) -> Option<MultiPartWriterStats> {
        self.stats.clone()
    }

    fn get_finished_file(&mut self) -> FileToFinish {
        self.multipart_manager.get_finished_file()
    }
}

impl<BB: BatchBuilder, BBW: BatchBufferingWriter<BatchData = BB::BatchData>>
    BatchMultipartWriter<BB, BBW>
{
    fn write_closing_multipart(
        &mut self,
    ) -> Result<Option<BoxedTryFuture<MultipartCallbackWithName>>> {
        self.multipart_manager.closed = true;
        let final_batch = if !self.batch_builder.buffered_inputs().is_empty() {
            Some(self.batch_builder.flush_buffer())
        } else {
            None
        };
        if let Some(bytes) = self.batch_buffering_writer.close(final_batch) {
            self.multipart_manager.write_next_part(bytes)
        } else if self.multipart_manager.all_uploads_finished() {
            // Return a finished file future
            let name = self.multipart_manager.name();
            Ok(Some(Box::pin(async move {
                Ok(MultipartCallbackWithName {
                    name,
                    callback: MultipartCallback::UploadsFinished,
                })
            })))
        } else {
            Ok(None)
        }
    }
}

struct PartToUpload {
    part_index: usize,
    byte_data: Vec<u8>,
}

#[derive(Debug)]
enum UploadPartOrBufferedData {
    UploadPart(UploadPart),
    BufferedData { data: Vec<u8> },
}

pub struct MultipartCallbackWithName {
    callback: MultipartCallback,
    name: String,
}

pub enum MultipartCallback {
    InitializedMultipart {
        multipart_id: MultipartId,
    },
    CompletedPart {
        part_idx: usize,
        upload_part: UploadPart,
    },
    UploadsFinished,
}

impl Debug for MultipartCallback {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            MultipartCallback::InitializedMultipart { .. } => {
                write!(f, "MultipartCallback::InitializedMultipart")
            }
            MultipartCallback::CompletedPart { part_idx, .. } => {
                write!(f, "MultipartCallback::CompletedPart({})", part_idx)
            }
            MultipartCallback::UploadsFinished => write!(f, "MultipartCallback::UploadsFinished"),
        }
    }
}

#[derive(Debug, Decode, Encode, Clone, PartialEq, Eq)]
pub struct FileSystemDataRecovery<T: Data> {
    next_file_index: usize,
    active_files: Vec<InProgressFileCheckpoint<T>>,
}

#[async_trait]
impl<K: Key, T: Data + Sync, R: MultiPartWriter<InputType = T> + Send + 'static>
    TwoPhaseCommitter<K, T> for FileSystemSink<K, T, R>
{
    type DataRecovery = FileSystemDataRecovery<T>;

    type PreCommit = FileToFinish;

    fn name(&self) -> String {
        "filesystem_sink".to_string()
    }

    async fn init(
        &mut self,
        task_info: &TaskInfo,
        data_recovery: Vec<Self::DataRecovery>,
    ) -> Result<()> {
        let mut max_file_index = 0;
        let mut recovered_files = Vec::new();
        for file_system_data_recovery in data_recovery {
            max_file_index = max_file_index.max(file_system_data_recovery.next_file_index);
            // task 0 is responsible for recovering all files.
            // This is because the number of subtasks may have changed.
            // Recovering should be reasonably fast since it is just finishing in-flight uploads.
            if task_info.task_index == 0 {
                recovered_files.extend(file_system_data_recovery.active_files.into_iter());
            }
        }
        self.sender
            .send(FileSystemMessages::Init {
                max_file_index,
                subtask_id: task_info.task_index,
                recovered_files,
            })
            .await?;
        Ok(())
    }

    async fn insert_record(&mut self, record: &Record<K, T>) -> Result<()> {
        let value = record.value.clone();
        self.sender
            .send(FileSystemMessages::Data {
                value,
                time: record.timestamp,
            })
            .await?;
        Ok(())
    }

    async fn commit(
        &mut self,
        _task_info: &TaskInfo,
        pre_commit: Vec<Self::PreCommit>,
    ) -> Result<()> {
        self.sender
            .send(FileSystemMessages::FilesToFinish(pre_commit))
            .await?;
        // loop over checkpoint receiver until finished received
        while let Some(checkpoint_message) = self.checkpoint_receiver.recv().await {
            match checkpoint_message {
                CheckpointData::Finished { max_file_index: _ } => return Ok(()),
                _ => {
                    bail!("unexpected checkpoint message")
                }
            }
        }
        bail!("checkpoint receiver closed unexpectedly")
    }

    async fn checkpoint(
        &mut self,
        task_info: &TaskInfo,
        stopping: bool,
    ) -> Result<(Self::DataRecovery, HashMap<String, Self::PreCommit>)> {
        self.sender
            .send(FileSystemMessages::Checkpoint {
                subtask_id: task_info.task_index,
                then_stop: stopping,
            })
            .await?;
        let mut pre_commit_messages = HashMap::new();
        let mut active_files = Vec::new();
        while let Some(checkpoint_message) = self.checkpoint_receiver.recv().await {
            match checkpoint_message {
                CheckpointData::Finished { max_file_index } => {
                    return Ok((
                        FileSystemDataRecovery {
                            next_file_index: max_file_index + 1,
                            active_files,
                        },
                        pre_commit_messages,
                    ))
                }
                CheckpointData::InProgressFileCheckpoint(InProgressFileCheckpoint {
                    filename,
                    data,
                    buffered_data,
                }) => {
                    if let FileCheckpointData::MultiPartWriterUploadCompleted {
                        multi_part_upload_id,
                        completed_parts,
                    } = data
                    {
                        pre_commit_messages.insert(
                            filename.clone(),
                            FileToFinish {
                                filename,
                                multi_part_upload_id,
                                completed_parts,
                            },
                        );
                    } else {
                        active_files.push(InProgressFileCheckpoint {
                            filename,
                            data,
                            buffered_data,
                        })
                    }
                }
            }
        }
        bail!("checkpoint receiver closed unexpectedly")
    }
}

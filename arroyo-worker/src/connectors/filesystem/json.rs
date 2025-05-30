use std::{fs::File, io::Write, marker::PhantomData};

use arroyo_types::Data;
use serde::Serialize;

use super::{
    local::{CurrentFileRecovery, LocalWriter},
    BatchBufferingWriter, BatchBuilder, FileSettings,
};

pub struct PassThrough<D: Data> {
    _phantom: PhantomData<D>,
}

impl<D: Data> BatchBuilder for PassThrough<D> {
    type InputType = D;

    type BatchData = D;

    fn new(_config: &super::FileSystemTable) -> Self {
        Self {
            _phantom: PhantomData,
        }
    }

    fn insert(&mut self, value: Self::InputType) -> Option<Self::BatchData> {
        Some(value)
    }

    fn buffered_inputs(&self) -> Vec<Self::InputType> {
        Vec::new()
    }

    fn flush_buffer(&mut self) -> Self::BatchData {
        unreachable!()
    }
}

pub struct JsonWriter<D: Data + Serialize> {
    current_buffer: Vec<u8>,
    target_part_size: usize,
    phantom: PhantomData<D>,
}

impl<D: Data + Serialize> BatchBufferingWriter for JsonWriter<D> {
    type BatchData = D;

    fn new(config: &super::FileSystemTable) -> Self {
        let target_part_size = if let Some(FileSettings {
            target_part_size: Some(target_part_size),
            ..
        }) = config.file_settings
        {
            target_part_size as usize
        } else {
            5 * 1024 * 1024
        };
        Self {
            current_buffer: Vec::new(),
            target_part_size,
            phantom: PhantomData,
        }
    }
    fn suffix() -> String {
        "json".to_string()
    }

    fn add_batch_data(&mut self, data: Self::BatchData) -> Option<Vec<u8>> {
        self.current_buffer
            .extend(serde_json::to_vec(&data).unwrap());
        self.current_buffer.extend(b"\n");
        if self.buffer_length() > self.target_part_size {
            Some(self.evict_current_buffer())
        } else {
            None
        }
    }

    fn buffer_length(&self) -> usize {
        self.current_buffer.len()
    }

    fn evict_current_buffer(&mut self) -> Vec<u8> {
        // take
        let result = std::mem::take(&mut self.current_buffer);
        result
    }

    fn get_trailing_bytes_for_checkpoint(&mut self) -> Option<Vec<u8>> {
        if self.current_buffer.is_empty() {
            None
        } else {
            Some(self.current_buffer.clone())
        }
    }

    fn close(&mut self, final_batch: Option<Self::BatchData>) -> Option<Vec<u8>> {
        if let Some(final_batch) = final_batch {
            if let Some(final_batch) = self.add_batch_data(final_batch) {
                return Some(final_batch);
            }
        }
        if self.current_buffer.is_empty() {
            None
        } else {
            Some(self.evict_current_buffer())
        }
    }
}

pub struct JsonLocalWriter {
    tmp_path: String,
    final_path: String,
    file: File,
}

impl<D: Data + Serialize> LocalWriter<D> for JsonLocalWriter {
    fn new(
        tmp_path: String,
        final_path: String,
        _table_properties: &super::FileSystemTable,
    ) -> Self {
        let file = File::create(&tmp_path).unwrap();
        JsonLocalWriter {
            tmp_path,
            final_path,
            file,
        }
    }

    fn file_suffix() -> &'static str {
        "json"
    }

    fn write(&mut self, value: D) -> anyhow::Result<()> {
        self.file
            .write_all(serde_json::to_vec(&value)?.as_slice())?;
        self.file.write_all(b"\n")?;
        Ok(())
    }

    fn sync(&mut self) -> anyhow::Result<usize> {
        self.file.flush()?;
        let size = self.file.metadata()?.len() as usize;
        Ok(size)
    }

    fn close(&mut self) -> anyhow::Result<super::local::FilePreCommit> {
        LocalWriter::<D>::sync(self)?;
        Ok(super::local::FilePreCommit {
            tmp_file: self.tmp_path.clone(),
            destination: self.final_path.clone(),
        })
    }

    fn checkpoint(&mut self) -> anyhow::Result<Option<super::local::CurrentFileRecovery>> {
        let bytes_written = LocalWriter::<D>::sync(self)?;
        if bytes_written > 0 {
            Ok(Some(CurrentFileRecovery {
                tmp_file: self.tmp_path.clone(),
                bytes_written,
                suffix: None,
                destination: self.final_path.clone(),
            }))
        } else {
            Ok(None)
        }
    }
}

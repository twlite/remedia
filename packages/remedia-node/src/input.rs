use napi::bindgen_prelude::{Buffer, Status};
use napi_derive::napi;
use remedia_core::InputSource;

use crate::native::NativeRemedia;
use crate::runtime::{lock, run_blocking};
use crate::util::napi_error;

const BYTE_STREAM_CAPACITY_BYTES: usize = 512 * 1024;

#[napi]
impl NativeRemedia {
    /// Create a bounded native byte input for one Node.js Readable producer.
    #[napi]
    pub fn create_byte_stream(&self) -> napi::Result<String> {
        self.inner.ensure_open()?;
        let (source, writer) = InputSource::byte_stream(BYTE_STREAM_CAPACITY_BYTES)
            .map_err(|error| napi_error(Status::InvalidArg, error.to_string()))?;
        let id = self.inner.id();
        lock(&self.inner.byte_streams).insert(
            id.clone(),
            crate::runtime::ByteStreamEntry { source, writer },
        );
        Ok(id)
    }

    /// Copy one Node buffer into the native-owned bounded input, applying
    /// backpressure when the decoder has not consumed enough pending bytes.
    #[napi]
    pub async fn write_byte_stream(&self, stream_id: String, chunk: Buffer) -> napi::Result<()> {
        let _operation = self.inner.operations.read().await;
        self.inner.ensure_open()?;
        let writer = self.inner.byte_stream(&stream_id)?.writer;
        let bytes = Vec::<u8>::from(chunk);
        let result = run_blocking(move || writer.write_owned(bytes)).await?;
        result.map_err(|error| napi_error(Status::GenericFailure, error.to_string()))
    }

    /// Signal a clean producer EOF. The stream handle remains valid until it is
    /// cancelled so playback can resolve its source if EOF arrives quickly.
    #[napi]
    pub async fn finish_byte_stream(&self, stream_id: String) -> napi::Result<()> {
        self.inner.ensure_open()?;
        let writer = self.inner.byte_stream(&stream_id)?.writer;
        let result = run_blocking(move || writer.finish()).await?;
        result.map_err(|error| napi_error(Status::GenericFailure, error.to_string()))
    }

    /// Propagate a JavaScript Readable error through FFmpeg to playback state.
    #[napi]
    pub async fn fail_byte_stream(&self, stream_id: String, message: String) -> napi::Result<()> {
        self.inner.ensure_open()?;
        let writer = self.inner.byte_stream(&stream_id)?.writer;
        let result = run_blocking(move || writer.fail(message)).await?;
        result.map_err(|error| napi_error(Status::GenericFailure, error.to_string()))
    }

    /// Cancel an input and wake any native readers or writers blocked by its
    /// bounded queue. Repeated calls are harmless.
    #[napi]
    pub fn cancel_byte_stream(&self, stream_id: String) -> napi::Result<()> {
        if let Some(stream) = lock(&self.inner.byte_streams).remove(&stream_id) {
            stream.writer.cancel();
        }
        Ok(())
    }
}

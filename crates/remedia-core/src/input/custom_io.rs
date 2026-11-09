use super::*;

pub(super) enum CustomReaderKind {
    Memory {
        bytes: std::sync::Arc<Vec<u8>>,
        offset: usize,
    },
    Stream {
        reader: ByteStreamReader,
        cancellation: CancellationToken,
    },
}

pub(super) struct CustomReader {
    pub(super) kind: CustomReaderKind,
}

impl CustomReader {
    pub(super) fn read(&mut self, output: &mut [u8]) -> Result<usize, ByteStreamError> {
        match &mut self.kind {
            CustomReaderKind::Memory { bytes, offset } => {
                let count = output.len().min(bytes.len().saturating_sub(*offset));
                if count > 0 {
                    output[..count].copy_from_slice(&bytes[*offset..*offset + count]);
                    *offset += count;
                }
                Ok(count)
            }
            CustomReaderKind::Stream {
                reader,
                cancellation,
            } => reader.read(output, cancellation),
        }
    }

    pub(super) fn seek(&mut self, offset: i64, whence: c_int) -> Result<i64, c_int> {
        let CustomReaderKind::Memory {
            bytes,
            offset: position,
        } = &mut self.kind
        else {
            return Err(ffmpeg::Error::Other {
                errno: ffmpeg::error::ESPIPE,
            }
            .into());
        };

        if whence & ffi::AVSEEK_SIZE != 0 {
            return i64::try_from(bytes.len()).map_err(|_| {
                ffmpeg::Error::Other {
                    errno: ffmpeg::error::EOVERFLOW,
                }
                .into()
            });
        }

        let origin = match whence & 0xffff {
            0 => 0_i128,              // SEEK_SET
            1 => *position as i128,   // SEEK_CUR
            2 => bytes.len() as i128, // SEEK_END
            _ => {
                return Err(ffmpeg::Error::Other {
                    errno: ffmpeg::error::EINVAL,
                }
                .into());
            }
        };
        let target = origin + i128::from(offset);
        if target < 0 || target > bytes.len() as i128 {
            return Err(ffmpeg::Error::Other {
                errno: ffmpeg::error::EINVAL,
            }
            .into());
        }
        *position = target as usize;
        i64::try_from(*position).map_err(|_| {
            ffmpeg::Error::Other {
                errno: ffmpeg::error::EOVERFLOW,
            }
            .into()
        })
    }

    pub(super) fn producer_failure(&self) -> Option<String> {
        match &self.kind {
            CustomReaderKind::Memory { .. } => None,
            CustomReaderKind::Stream { reader, .. } => reader.failure(),
        }
    }
}

pub(super) struct CustomIo {
    pub(super) context: *mut ffi::AVIOContext,
    pub(super) reader: Box<CustomReader>,
}

// SAFETY: `context` is uniquely owned by this value and is never accessed by
// another thread concurrently. Moving ownership is safe; the opaque callback
// data points to the separately boxed reader, whose address remains stable.
unsafe impl Send for CustomIo {}

impl CustomIo {
    pub(super) fn memory(bytes: std::sync::Arc<Vec<u8>>) -> Result<Self, DecoderError> {
        Self::new(CustomReader {
            kind: CustomReaderKind::Memory { bytes, offset: 0 },
        })
    }

    pub(super) fn stream(
        source: &crate::byte_stream::ByteStreamInput,
        cancellation: CancellationToken,
    ) -> Result<Self, DecoderError> {
        let reader = source.reader();
        reader.register_cancellation(&cancellation);
        Self::new(CustomReader {
            kind: CustomReaderKind::Stream {
                reader,
                cancellation,
            },
        })
    }

    pub(super) fn new(reader: CustomReader) -> Result<Self, DecoderError> {
        let mut reader = Box::new(reader);
        // FFmpeg requires this buffer to use its allocator. It may replace the
        // buffer while probing, so `Drop` frees the pointer currently in the
        // AVIOContext rather than the original pointer.
        let buffer = unsafe { ffi::av_malloc(AVIO_BUFFER_BYTES) }.cast::<u8>();
        if buffer.is_null() {
            return Err(DecoderError::OpenInput(
                "could not allocate an FFmpeg I/O buffer".to_owned(),
            ));
        }
        let opaque = (&mut *reader as *mut CustomReader).cast::<c_void>();
        let memory_source = matches!(reader.kind, CustomReaderKind::Memory { .. });
        let context = unsafe {
            ffi::avio_alloc_context(
                buffer,
                AVIO_BUFFER_BYTES as c_int,
                0,
                opaque,
                Some(read_packet_callback),
                None,
                memory_source.then_some(seek_packet_callback),
            )
        };
        if context.is_null() {
            unsafe { ffi::av_free(buffer.cast::<c_void>()) };
            return Err(DecoderError::OpenInput(
                "could not allocate an FFmpeg I/O context".to_owned(),
            ));
        }
        // SAFETY: `context` was allocated by `avio_alloc_context` above.
        unsafe {
            (*context).seekable = if memory_source {
                ffi::AVIO_SEEKABLE_NORMAL
            } else {
                0
            };
        }
        Ok(Self { context, reader })
    }

    pub(super) fn as_ptr(&mut self) -> *mut ffi::AVIOContext {
        self.context
    }

    pub(super) fn producer_failure(&self) -> Option<String> {
        self.reader.producer_failure()
    }
}

impl Drop for CustomIo {
    fn drop(&mut self) {
        if self.context.is_null() {
            return;
        }

        // SAFETY: this is the live AVIOContext allocation returned by
        // `avio_alloc_context`. FFmpeg may replace `buffer` during reads, so
        // free the current field before freeing the context itself.
        unsafe {
            ffi::av_freep((&mut (*self.context).buffer as *mut *mut u8).cast::<c_void>());
            ffi::avio_context_free(&mut self.context);
        }
    }
}

unsafe extern "C" fn read_packet_callback(
    opaque: *mut c_void,
    output: *mut u8,
    output_size: c_int,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if opaque.is_null() || output.is_null() || output_size <= 0 {
            return ffmpeg::Error::Other {
                errno: ffmpeg::error::EINVAL,
            }
            .into();
        }

        // SAFETY: `opaque` points to the boxed reader owned by `CustomIo` and
        // remains stable until after the AVIO context is freed. FFmpeg invokes
        // this callback synchronously on the owning decoder worker.
        let reader = unsafe { &mut *opaque.cast::<CustomReader>() };
        // SAFETY: FFmpeg supplies a writable buffer for `output_size` bytes.
        let output = unsafe { slice::from_raw_parts_mut(output, output_size as usize) };
        match reader.read(output) {
            Ok(0) => ffmpeg::Error::Eof.into(),
            Ok(count) => c_int::try_from(count).unwrap_or(c_int::MAX),
            Err(ByteStreamError::Cancelled | ByteStreamError::ConsumerClosed) => {
                ffmpeg::Error::Exit.into()
            }
            Err(ByteStreamError::ProducerFailed(_)) => ffmpeg::Error::External.into(),
            Err(_) => ffmpeg::Error::External.into(),
        }
    }))
    .unwrap_or_else(|_| ffmpeg::Error::External.into())
}

unsafe extern "C" fn seek_packet_callback(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    catch_unwind(AssertUnwindSafe(|| {
        if opaque.is_null() {
            return i64::from(c_int::from(ffmpeg::Error::Other {
                errno: ffmpeg::error::EINVAL,
            }));
        }
        // SAFETY: `opaque` points to the boxed reader owned by `CustomIo`; the
        // memory input's position is mutated only by this decoder's callbacks.
        let reader = unsafe { &mut *opaque.cast::<CustomReader>() };
        reader.seek(offset, whence).unwrap_or_else(i64::from)
    }))
    .unwrap_or_else(|_| i64::from(c_int::from(ffmpeg::Error::External)))
}

pub(super) fn custom_io_for(
    source: &InputSource,
    cancellation: CancellationToken,
) -> Result<Option<CustomIo>, DecoderError> {
    match source {
        InputSource::Bytes(bytes) => CustomIo::memory(Arc::clone(bytes)).map(Some),
        InputSource::ByteStream(source) => CustomIo::stream(source, cancellation).map(Some),
        InputSource::File(_) | InputSource::HttpUrl(_) | InputSource::SequentialHttpUrl(_) => {
            Ok(None)
        }
    }
}

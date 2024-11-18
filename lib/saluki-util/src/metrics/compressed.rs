use saluki_error::GenericError;
use vortex_array::{array::StructArray, ArrayData};
use vortex_file::VortexFileWriter;
use vortex_io::VortexWrite;
use vortex_sampling_compressor::{compressors::CompressorRef, SamplingCompressor};

#[repr(C)]
struct UncompressedBlock<const N: usize> {
    series_id: [u32; N],
    ts: [u32; N],
    point: [f64; N],
    len: usize,
}

impl<const N: usize> UncompressedBlock<N> {
    const fn new() -> Self {
        Self {
            series_id: [0; N],
            ts: [0; N],
            point: [0.0; N],
            len: 0,
        }
    }

    const fn len(&self) -> usize {
        self.len
    }

    const fn is_full(&self) -> bool {
        self.len == N
    }

    fn add_point(&mut self, series_id: u32, ts: u32, point: f64) {
        if self.is_full() {
            panic!("block is full");
        }

        self.series_id[self.len] = series_id;
        self.ts[self.len] = ts;
        self.point[self.len] = point;

        self.len += 1;
    }

    fn get_point(&self, idx: usize) -> Option<(u32, u32, f64)> {
        if idx.saturating_add(1) > self.len() {
            return None;
        }

        let series_id = self.series_id[idx];
        let ts = self.ts[idx];
        let point = self.point[idx];

        Some((series_id, ts, point))
    }

    fn reset(&mut self) {
        self.len = 0;
    }

    async fn write_compressed<W: VortexWrite>(&self, writer: &mut W) -> Result<usize, GenericError> {
        // Arrange our points into a struct array, and then run the sampling compressor over it.
        let series_id = ArrayData::from(self.series_id[..self.len].to_vec());
        let ts = ArrayData::from(self.ts[..self.len].to_vec());
        let point = ArrayData::from(self.point[..self.len].to_vec());

        let points = StructArray::from_fields(&[
            ("series_id", series_id),
            ("ts", ts),
            ("point", point),
        ])?;

         // Compress our points with floating-point optimized compressors.
        let compressor = get_floating_point_sampling_compressor();
        let compressed_points = compressor.compress_array(points.as_ref())?;

        // Write the compressed points to the writer.
        let file_writer = VortexFileWriter::new(writer);
        file_writer.write_array_columns(compressed_points.into_array()).await?
            .finalize().await?;

        // TODO: I think one thing to do/try would be if we could somehow build the context/data type stuff manually to
        // allow serializing each column directly, and then deserializing it, without going through all of the
        // rigamarole of `VortexFileWriter`... and maybe that could somehow save us all of the metadata overhead that
        // seems to be getting tacked on?

        Ok(self.len)
    }
}

fn get_integer_sampling_compressor() -> SamplingCompressor<'static> {
    SamplingCompressor::new([
        &vortex_sampling_compressor::compressors::bitpacked::BITPACK_WITH_PATCHES as CompressorRef,
        &vortex_sampling_compressor::compressors::delta::DeltaCompressor as CompressorRef,
        &vortex_sampling_compressor::compressors::r#for::FoRCompressor as CompressorRef,
        &vortex_sampling_compressor::compressors::roaring_int::RoaringIntCompressor as CompressorRef,
    ].into())
}

fn get_floating_point_sampling_compressor() -> SamplingCompressor<'static> {
    SamplingCompressor::new([
        &vortex_sampling_compressor::compressors::alp::ALPCompressor as CompressorRef,
        &vortex_sampling_compressor::compressors::alp_rd::ALPRDCompressor as CompressorRef,
        &vortex_sampling_compressor::compressors::bitpacked::BITPACK_WITH_PATCHES as CompressorRef,
        &vortex_sampling_compressor::compressors::delta::DeltaCompressor as CompressorRef,
        &vortex_sampling_compressor::compressors::r#for::FoRCompressor as CompressorRef,
        &vortex_sampling_compressor::compressors::roaring_int::RoaringIntCompressor as CompressorRef,
    ].into())
}

pub struct CompressedColumnarMetrics {
    compressed: Vec<u8>,
    compressed_points: usize,
    uncompressed: UncompressedBlock<1024>,
}

impl CompressedColumnarMetrics {
    pub fn new() -> Self {
        Self {
            compressed: Vec::new(),
            compressed_points: 0,
            uncompressed: UncompressedBlock::new(),
        }
    }

    pub const fn len(&self) -> usize {
        self.compressed_points + self.uncompressed.len()
    }

    pub fn len_bytes(&self) -> usize {
        self.compressed.len() + (self.uncompressed.len() * std::mem::size_of::<(u32, u32, f64)>())
    }

    pub async fn add_point(&mut self, series_id: u32, ts: u32, point: f64) -> Result<(), GenericError> {
        if self.uncompressed.is_full() {
            self.compress_and_flush().await?;
        }

        self.uncompressed.add_point(series_id, ts, point);

        Ok(())
    }

    async fn compress_and_flush(&mut self) -> Result<(), GenericError> {
        let before_len = self.compressed.len();
        let points = self.uncompressed.write_compressed(&mut self.compressed).await?;
        let delta = self.compressed.len() - before_len;

        println!("compressed {} points into {} bytes", points, delta);
        self.compressed_points += points;

        self.uncompressed.reset();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rand::Rng;

    #[tokio::test]
    async fn test_compressed_size() {
        let num_series = 500u32;
        let num_seconds = 300;
        let total_points = num_series * num_seconds;

        let ts_start = 1731893063;
        let ts_end = ts_start + num_seconds;

        let mut metrics = CompressedColumnarMetrics::new();
        let mut rng = rand::thread_rng();

        for _ in 0..total_points {
            let series_id = rng.gen_range(0..num_series);
            let ts = rng.gen_range(ts_start..ts_end);
            let point = rng.gen_range(0.0..100.0);

            metrics.add_point(series_id, ts, point).await.expect("failed to add point");
        }

        println!("total points: {}, bytes: {}", metrics.len(), metrics.len_bytes());
    }
}

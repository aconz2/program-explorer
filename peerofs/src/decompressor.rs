pub trait Decompressor {
    fn decompress(&self, _src: &[u8], _dst: &mut [u8], _original_size: usize) -> Option<usize> {
        None
    }
}

#[allow(dead_code)]
pub struct Lz4Decompressor;

impl Decompressor for Lz4Decompressor {
    #[cfg(feature = "lz4")]
    fn decompress(&self, src: &[u8], dst: &mut [u8], original_size: usize) -> Option<usize> {
        lzzzz::lz4::decompress_partial(src, dst, original_size).ok()
    }
}

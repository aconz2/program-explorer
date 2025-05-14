use oci_spec::image::MediaType;

#[derive(Debug)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

impl TryFrom<&MediaType> for Compression {
    type Error = ();
    fn try_from(x: &MediaType) -> Result<Compression, Self::Error> {
        match x {
            MediaType::ImageLayer => Ok(Compression::None),
            MediaType::ImageLayerGzip => Ok(Compression::Gzip),
            MediaType::ImageLayerZstd => Ok(Compression::Zstd),
            _ => Err(()),
        }
    }
}

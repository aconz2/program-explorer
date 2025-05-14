use oci_spec::image::{Descriptor, MediaType};

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

impl TryFrom<&Descriptor> for Compression {
    type Error = ();
    fn try_from(x: &Descriptor) -> Result<Compression, Self::Error> {
        match x.media_type() {
            // is this a thing? I don't think so
            //MediaType::Other(s) if s == "application/vnd.docker.image.rootfs.diff.tar" => Compression::None,
            MediaType::Other(s) if s == "application/vnd.docker.image.rootfs.diff.tar.gzip" => {
                Ok(Compression::Gzip)
            }
            // I don't think this ever made its way into the wild?
            //MediaType::Other(s) if s == "application/vnd.docker.image.rootfs.diff.tar.zstd" => Compression::Zstd,
            MediaType::ImageManifest => x.artifact_type().as_ref().ok_or(())?.try_into(),
            _ => Err(()),
        }
    }
}

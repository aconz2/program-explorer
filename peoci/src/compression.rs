use oci_spec::image::{Descriptor, MediaType};

#[derive(Debug)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

#[derive(Debug, thiserror::Error)]
pub struct Error {
    pub media_type: MediaType,
    pub artifact_type: Option<MediaType>,
}

// how wrong is this?
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

// OCI Descriptor for an image layer has two fields, mediaType and artifactType.
// mediaType is the "old" docker style media type
// artifactType is set for ... (I'm not sure I actually understand this

// so I originally had this, then add TryFrom<&Descriptor> because I hit a weird case that required
// inspecting artifact_type, but maybe I didn't? Anyways just leaving from Descriptor for now
// because it is more general and means you can just pass a layer (Descriptor)

//impl TryFrom<&MediaType> for Compression {
//    type Error = ();
//    fn try_from(x: &MediaType) -> Result<Compression, Self::Error> {
//        match x {
//            MediaType::ImageLayer => Ok(Compression::None),
//            MediaType::ImageLayerGzip => Ok(Compression::Gzip),
//            MediaType::ImageLayerZstd => Ok(Compression::Zstd),
//            _ => Err(()),
//        }
//    }
//}

impl TryFrom<&Descriptor> for Compression {
    type Error = Error;
    fn try_from(x: &Descriptor) -> Result<Compression, Self::Error> {
        match (x.media_type(), x.artifact_type()) {
            // is this a thing? I don't think so
            //MediaType::Other(s) if s == "application/vnd.docker.image.rootfs.diff.tar" => Compression::None,
            (MediaType::Other(s), _)
                if s == "application/vnd.docker.image.rootfs.diff.tar.gzip" =>
            {
                Ok(Compression::Gzip)
            }

            // I don't think this ever made its way into the wild?
            //MediaType::Other(s) if s == "application/vnd.docker.image.rootfs.diff.tar.zstd" => Compression::Zstd,
            (MediaType::ImageLayer, _) => Ok(Compression::None),
            (MediaType::ImageLayerGzip, _) => Ok(Compression::Gzip),
            (MediaType::ImageLayerZstd, _) => Ok(Compression::Zstd),
            (media_type, artifact_type) => Err(Error {
                media_type: media_type.clone(),
                artifact_type: artifact_type.clone(),
            }),
        }
    }
}

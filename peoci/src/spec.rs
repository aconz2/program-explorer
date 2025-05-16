use bincode::{Decode, Encode};

// this is a redux version of some oci_spec types that implement bincode::Encode/Decode with
// borrowing, we omit some fields to save space in the cache

#[derive(Debug, thiserror::Error)]
pub enum Error {
    UnhandledMediaType(String),
    BadDigest,
    UnhandledDigest(String),
    UnhandledOs(String),
    UnhandledArch(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Encode, Decode, Copy, Clone)]
pub enum Os {
    Linux,
}

#[derive(Debug, Encode, Decode, Copy, Clone)]
pub enum Arch {
    Amd64,
    Arm64,
}

#[derive(Debug, Encode, Decode, Copy, Clone)]
pub enum MediaType {
    ImageLayer,
    ImageLayerGzip,
    ImageLayerZstd,
    DockerImageLayerGzip,
}

#[derive(Debug, Encode, Decode, Copy, Clone)]
pub enum Digest {
    Sha256([u8; 32]),
}

#[derive(Debug, Encode, Decode, Clone)]
pub struct ImageManifest {
    pub layers: Vec<LayerDescriptor>,
}

#[derive(Debug, Encode, Decode, Copy, Clone)]
pub struct LayerDescriptor {
    pub media_type: MediaType,
    pub digest: Digest,
    pub size: u64,
}

#[derive(Debug, Encode, Decode)]
pub struct ImageConfiguration {
    pub architecture: Arch,
    pub os: Os,
    pub config: Option<Config>,
}

#[derive(Debug, Encode, Decode)]
pub struct ImageManifestAndConfiguration {
    pub manifest_digest: Digest,
    pub manifest: ImageManifest,
    pub configuration: ImageConfiguration,
}

#[derive(Debug, Encode, Decode)]
pub struct Config {
    pub user: Option<String>,
    pub exposed_ports: Option<Vec<String>>,
    pub env: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub stop_signal: Option<String>,
}

impl TryFrom<&oci_spec::image::Os> for Os {
    type Error = Error;
    fn try_from(os: &oci_spec::image::Os) -> Result<Self, Error> {
        use oci_spec::image::Os as O;
        match os {
            O::Linux => Ok(Os::Linux),
            os => Err(Error::UnhandledOs(os.to_string())),
        }
    }
}

impl TryFrom<&oci_spec::image::Arch> for Arch {
    type Error = Error;
    fn try_from(arch: &oci_spec::image::Arch) -> Result<Self, Error> {
        use oci_spec::image::Arch as O;
        match arch {
            O::Amd64 => Ok(Arch::Amd64),
            O::ARM64 => Ok(Arch::Arm64),
            arch => Err(Error::UnhandledArch(arch.to_string())),
        }
    }
}

impl TryFrom<&oci_spec::image::ImageManifest> for ImageManifest {
    type Error = Error;
    fn try_from(image: &oci_spec::image::ImageManifest) -> Result<Self, Error> {
        Ok(Self {
            layers: image
                .layers()
                .iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

impl TryFrom<&oci_spec::image::Descriptor> for LayerDescriptor {
    type Error = Error;
    fn try_from(descriptor: &oci_spec::image::Descriptor) -> Result<Self, Error> {
        Ok(Self {
            media_type: descriptor.media_type().try_into()?,
            digest: descriptor.digest().try_into()?,
            size: descriptor.size(),
        })
    }
}

impl TryFrom<&oci_spec::image::Digest> for Digest {
    type Error = Error;
    fn try_from(digest: &oci_spec::image::Digest) -> Result<Self, Error> {
        use oci_spec::image::DigestAlgorithm;
        match digest.algorithm() {
            DigestAlgorithm::Sha256 => Ok(Digest::Sha256(hex_decode::<32>(digest.digest())?)),
            a => Err(Error::UnhandledDigest(a.to_string())),
        }
    }
}

impl TryFrom<&str> for Digest {
    type Error = Error;
    fn try_from(s: &str) -> Result<Self, Error> {
        match s.split_once(':') {
            Some(("sha256", data)) => Ok(Digest::Sha256(hex_decode::<32>(data)?)),
            Some((digest, _)) => Err(Error::UnhandledDigest(digest.to_string())),
            _ => Err(Error::BadDigest),
        }
    }
}

impl TryFrom<&oci_spec::image::MediaType> for MediaType {
    type Error = Error;
    fn try_from(mt: &oci_spec::image::MediaType) -> Result<Self, Error> {
        use oci_spec::image::MediaType as M;
        match mt {
            M::ImageLayer => Ok(MediaType::ImageLayer),
            M::ImageLayerGzip => Ok(MediaType::ImageLayerGzip),
            M::ImageLayerZstd => Ok(MediaType::ImageLayerZstd),
            M::Other(s) if s == "application/vnd.docker.image.rootfs.diff.tar.gzip" => {
                Ok(MediaType::DockerImageLayerGzip)
            }
            m => Err(Error::UnhandledMediaType(m.to_string())),
        }
    }
}

impl TryFrom<&oci_spec::image::ImageConfiguration> for ImageConfiguration {
    type Error = Error;
    fn try_from(ic: &oci_spec::image::ImageConfiguration) -> Result<Self, Error> {
        Ok(Self {
            architecture: ic.architecture().try_into()?,
            os: ic.os().try_into()?,
            config: ic.config().as_ref().map(TryInto::try_into).transpose()?,
        })
    }
}

impl TryFrom<&oci_spec::image::Config> for Config {
    type Error = Error;
    fn try_from(ic: &oci_spec::image::Config) -> Result<Self, Error> {
        Ok(Self {
            user: ic.user().clone(),
            exposed_ports: ic.exposed_ports().clone(),
            env: ic.env().clone(),
            entrypoint: ic.entrypoint().clone(),
            cmd: ic.cmd().clone(),
            working_dir: ic.working_dir().clone(),
            stop_signal: ic.stop_signal().clone(),
        })
    }
}

impl From<LayerDescriptor> for oci_spec::image::Descriptor {
    fn from(descriptor: LayerDescriptor) -> oci_spec::image::Descriptor {
        oci_spec::image::Descriptor::new(
            descriptor.media_type.into(),
            descriptor.size,
            descriptor.digest,
        )
    }
}

impl From<MediaType> for oci_spec::image::MediaType {
    fn from(media_type: MediaType) -> oci_spec::image::MediaType {
        use oci_spec::image::MediaType as M;
        match media_type {
            MediaType::ImageLayer => M::ImageLayer,
            MediaType::ImageLayerGzip => M::ImageLayerGzip,
            MediaType::ImageLayerZstd => M::ImageLayerZstd,
            MediaType::DockerImageLayerGzip => {
                M::Other("application/vnd.docker.image.rootfs.diff.tar.gzip".to_string())
            }
        }
    }
}

impl From<Digest> for oci_spec::image::Digest {
    fn from(digest: Digest) -> oci_spec::image::Digest {
        match digest {
            Digest::Sha256(data) => hex::encode(data)
                .parse::<oci_spec::image::Sha256Digest>()
                .unwrap()
                .into(),
        }
    }
}

fn hex_decode<const N: usize>(s: &str) -> Result<[u8; N], Error> {
    let mut ret = [0; N];
    hex::decode_to_slice(s, &mut ret).map_err(|_| Error::BadDigest)?;
    Ok(ret)
}
